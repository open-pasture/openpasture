-- op-ingest (protocol v1): the report path reads a herd's active and staged
-- boundaries and a collar's own ones by (herd, collar, version); the
-- activation watcher looks boundaries up by effective_at; new versions come
-- from MAX(version) over one sequence, and collar slots join boundaries on
-- version.
CREATE INDEX boundaries_herd_collar_version ON boundaries(herd_id, collar_id, version);
CREATE INDEX boundaries_effective ON boundaries(effective_at);
CREATE INDEX boundaries_version ON boundaries(version);
