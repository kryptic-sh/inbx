ALTER TABLE contacts ADD COLUMN pgp_pubkey      TEXT;     -- ASCII-armored public key
ALTER TABLE contacts ADD COLUMN pgp_fingerprint TEXT;     -- hex fingerprint, denormalized for fast lookup
ALTER TABLE contacts ADD COLUMN pgp_seen_unix   INTEGER;  -- when we last saw an Autocrypt header for them
