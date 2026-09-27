-- P: health rolls up to Parquet by day like fixes and cues; the rollup finds
-- and deletes a day's rows by time.
CREATE INDEX IF NOT EXISTS health_t ON health(t);
