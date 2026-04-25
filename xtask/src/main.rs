use anyhow::Result;

fn main() -> Result<()> {
    let task = std::env::args().nth(1);
    match task.as_deref() {
        Some("help") | None => {
            println!("xtask: no tasks defined yet");
            Ok(())
        }
        Some(other) => {
            anyhow::bail!("unknown task: {other}");
        }
    }
}
