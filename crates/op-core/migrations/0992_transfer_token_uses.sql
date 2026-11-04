-- Single-use tracking for signed cross-farm transfer tokens (receiving farm).
CREATE TABLE transfer_token_uses (
    code_hash TEXT PRIMARY KEY,
    serial TEXT NOT NULL,
    collar_id TEXT NOT NULL,
    farm_pubkey TEXT NOT NULL,
    used_at TEXT NOT NULL
);
