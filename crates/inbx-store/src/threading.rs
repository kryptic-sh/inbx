use sqlx::SqlitePool;

use crate::Result;

pub(crate) struct Threader<'a> {
    pool: &'a SqlitePool,
}

impl<'a> Threader<'a> {
    pub(crate) fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert / update a container for `message_id`, link its References
    /// chain, run loose Subject grouping, recompute root_id for the
    /// affected subtree, and return the resolved thread_id (= root_id).
    pub(crate) async fn ingest(
        &self,
        message_id: &str,
        in_reply_to: Option<&str>,
        refs: &[String],
        subject: Option<&str>,
    ) -> Result<String> {
        let subject_norm = subject.map(normalize_subject);
        let subject_norm_ref = subject_norm.as_deref();

        // Step 2: upsert main container with has_message = 1.
        sqlx::query(
            "INSERT INTO thread_containers (message_id, root_id, subject_norm, has_message)
             VALUES (?1, ?1, ?2, 1)
             ON CONFLICT(message_id) DO UPDATE SET
                subject_norm = COALESCE(excluded.subject_norm, thread_containers.subject_norm),
                has_message  = 1",
        )
        .bind(message_id)
        .bind(subject_norm_ref)
        .execute(self.pool)
        .await?;

        // Step 3: upsert placeholder containers for each ref and link chain.
        // Build the full chain: refs in order, then in_reply_to if not already present.
        let mut chain: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
        if let Some(irt) = in_reply_to.filter(|irt| !chain.contains(irt)) {
            chain.push(irt);
        }

        // Upsert placeholder containers for all referenced ids.
        for &ref_id in &chain {
            sqlx::query(
                "INSERT INTO thread_containers (message_id, root_id, has_message)
                 VALUES (?1, ?1, 0)
                 ON CONFLICT(message_id) DO NOTHING",
            )
            .bind(ref_id)
            .execute(self.pool)
            .await?;
        }

        // Link consecutive pairs in the chain: chain[i] is parent of chain[i+1].
        // Only set parent when the child's parent is currently NULL and no cycle.
        for window in chain.windows(2) {
            let parent_id = window[0];
            let child_id = window[1];
            // Only set if child has no parent yet.
            let existing: Option<(Option<String>,)> =
                sqlx::query_as("SELECT parent_id FROM thread_containers WHERE message_id = ?1")
                    .bind(child_id)
                    .fetch_optional(self.pool)
                    .await?;
            if matches!(existing, Some((None,)))
                && !self.would_create_cycle(child_id, parent_id).await?
            {
                // Cycle check: walk up from proposed parent; if we hit child_id, skip.
                sqlx::query("UPDATE thread_containers SET parent_id = ?2 WHERE message_id = ?1")
                    .bind(child_id)
                    .bind(parent_id)
                    .execute(self.pool)
                    .await?;
            }
        }

        // Step 4: set parent of the new message container from last ref / in_reply_to.
        if let Some(&direct_parent) = chain.last() {
            let existing: Option<(Option<String>,)> =
                sqlx::query_as("SELECT parent_id FROM thread_containers WHERE message_id = ?1")
                    .bind(message_id)
                    .fetch_optional(self.pool)
                    .await?;
            if matches!(existing, Some((None,)))
                && !self.would_create_cycle(message_id, direct_parent).await?
            {
                sqlx::query("UPDATE thread_containers SET parent_id = ?2 WHERE message_id = ?1")
                    .bind(message_id)
                    .bind(direct_parent)
                    .execute(self.pool)
                    .await?;
            }
        }

        // Step 5: loose Subject grouping.
        // Only if: container has no parent AND subject_norm is set AND another root
        // with the same subject_norm exists AND the subject on at least one side
        // starts with a Re:/Fwd: prefix.
        let has_parent: Option<(Option<String>,)> =
            sqlx::query_as("SELECT parent_id FROM thread_containers WHERE message_id = ?1")
                .bind(message_id)
                .fetch_optional(self.pool)
                .await?;
        let is_root = matches!(has_parent, Some((None,)));

        #[allow(clippy::collapsible_if)]
        if is_root {
            if let Some(ref norm) = subject_norm {
                let this_is_reply = subject.map(is_reply_or_fwd).unwrap_or(false);
                // Find another root container with the same subject_norm, different id.
                // Tie-break by rowid (insertion order = oldest first).
                let other: Option<(String, Option<String>)> = sqlx::query_as(
                    "SELECT tc.message_id, m.subject
                     FROM thread_containers tc
                     LEFT JOIN messages m ON m.message_id = tc.message_id
                     WHERE tc.subject_norm = ?1
                       AND tc.message_id  != ?2
                       AND tc.parent_id   IS NULL
                     ORDER BY tc.rowid ASC
                     LIMIT 1",
                )
                .bind(norm.as_str())
                .bind(message_id)
                .fetch_optional(self.pool)
                .await?;

                if let Some((other_id, other_subj)) = other {
                    let other_is_reply =
                        other_subj.as_deref().map(is_reply_or_fwd).unwrap_or(false);
                    // At least one side must be a reply to trigger grouping.
                    if (this_is_reply || other_is_reply)
                        && !self.would_create_cycle(message_id, &other_id).await?
                    {
                        // Make the newer one a child of the older one.
                        // `other_id` is the older one (ORDER BY rowid ASC LIMIT 1).
                        sqlx::query(
                            "UPDATE thread_containers SET parent_id = ?2 WHERE message_id = ?1",
                        )
                        .bind(message_id)
                        .bind(&other_id)
                        .execute(self.pool)
                        .await?;
                    }
                }
            }
        }

        // Step 6: walk up to root.
        let root_id = self.find_root(message_id).await?;

        // Step 7: update root_id for the whole subtree if root changed.
        // First get the old root_id for the message container.
        let old_root: Option<(String,)> =
            sqlx::query_as("SELECT root_id FROM thread_containers WHERE message_id = ?1")
                .bind(message_id)
                .fetch_optional(self.pool)
                .await?;
        let old_root_id = old_root
            .map(|(r,)| r)
            .unwrap_or_else(|| message_id.to_string());

        if old_root_id != root_id {
            // Use WITH RECURSIVE to update all containers whose subtree root was old_root_id.
            // We update every container reachable from the new root down.
            sqlx::query(
                "WITH RECURSIVE subtree(id) AS (
                     SELECT message_id FROM thread_containers WHERE message_id = ?1
                     UNION ALL
                     SELECT tc.message_id
                     FROM thread_containers tc
                     JOIN subtree s ON tc.parent_id = s.id
                 )
                 UPDATE thread_containers
                 SET root_id = ?1
                 WHERE message_id IN (SELECT id FROM subtree)",
            )
            .bind(&root_id)
            .execute(self.pool)
            .await?;

            // Also update old subtree that was previously rooted at old_root_id.
            sqlx::query(
                "WITH RECURSIVE subtree(id) AS (
                     SELECT message_id FROM thread_containers WHERE root_id = ?1
                     UNION ALL
                     SELECT tc.message_id
                     FROM thread_containers tc
                     JOIN subtree s ON tc.parent_id = s.id
                 )
                 UPDATE thread_containers
                 SET root_id = ?2
                 WHERE message_id IN (SELECT id FROM subtree)",
            )
            .bind(&old_root_id)
            .bind(&root_id)
            .execute(self.pool)
            .await?;
        }

        // Step 8: update messages.thread_id for every message under the new root.
        sqlx::query(
            "UPDATE messages
             SET thread_id = ?1
             WHERE message_id IN (
                 SELECT message_id FROM thread_containers WHERE root_id = ?1
             )",
        )
        .bind(&root_id)
        .execute(self.pool)
        .await?;

        Ok(root_id)
    }

    /// Walk parent_id chain from `start` upward.
    /// Returns the message_id of the root (the one with parent_id IS NULL).
    async fn find_root(&self, start: &str) -> Result<String> {
        let mut current = start.to_string();
        // Guard against cycles (shouldn't happen after cycle checks, but be safe).
        for _ in 0..1000 {
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT parent_id FROM thread_containers WHERE message_id = ?1")
                    .bind(&current)
                    .fetch_optional(self.pool)
                    .await?;
            match row {
                Some((Some(parent),)) => current = parent,
                _ => break,
            }
        }
        Ok(current)
    }

    /// Returns true if setting `node`'s parent to `proposed_parent` would
    /// create a cycle. We walk upward from `proposed_parent`; if we reach
    /// `node`, it's a cycle.
    async fn would_create_cycle(&self, node: &str, proposed_parent: &str) -> Result<bool> {
        let mut current = proposed_parent.to_string();
        for _ in 0..1000 {
            if current == node {
                return Ok(true);
            }
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT parent_id FROM thread_containers WHERE message_id = ?1")
                    .bind(&current)
                    .fetch_optional(self.pool)
                    .await?;
            match row {
                Some((Some(parent),)) => current = parent,
                _ => break,
            }
        }
        Ok(false)
    }
}

/// Strip Re:/Fwd:/Re[2]: variants and leading/trailing whitespace; lowercase.
/// Used by Subject grouping.
pub fn normalize_subject(s: &str) -> String {
    let mut result = s.trim();
    loop {
        let lower = result.to_ascii_lowercase();
        if lower.starts_with("re:") {
            result = result[3..].trim();
        } else if lower.starts_with("fwd:") {
            result = result[4..].trim();
        } else if lower.starts_with("fw:") {
            result = result[3..].trim();
        } else if lower.starts_with("re[") {
            // Handle Re[N]: patterns
            if let Some(bracket_end) = lower.find("]:") {
                let skip = bracket_end + 2;
                result = result[skip..].trim();
            } else {
                break;
            }
        } else {
            break;
        }
    }
    result.to_ascii_lowercase()
}

/// Returns true if the subject line starts with a Re: or Fwd: prefix.
fn is_reply_or_fwd(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    lower.starts_with("re:")
        || lower.starts_with("fwd:")
        || lower.starts_with("fw:")
        || (lower.starts_with("re[") && lower.contains("]:"))
}
