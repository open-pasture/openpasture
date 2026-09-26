-- op-engine: at most one running brain decision per herd, so two starts can't
-- race. (MCP proposals have no `brain` and pass through `running` briefly.)
-- Anything still running when this migration runs was cut off by a restart.
UPDATE decisions SET status = 'failed', error = 'Interrupted by a restart.' WHERE status = 'running';
CREATE UNIQUE INDEX decisions_one_running ON decisions(herd_id) WHERE status = 'running' AND brain IS NOT NULL;
