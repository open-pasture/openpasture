-- op-alerts (FX-AL1): the collars an alert had when people were last told
-- about it (a text went out, or someone acked or closed it). Animals that
-- join a herd rollup after that are a new breakout once they are as many as
-- the told ones still in it (after a close: `rollup_min` of them), and it is
-- sent again as a new alert. JSON [collar id, …]; NULL until then.
ALTER TABLE alerts ADD COLUMN announced TEXT;
UPDATE alerts SET announced = (
    SELECT json_group_array(json_extract(value, '$[1]')) FROM json_each(alerts.targets) WHERE json_extract(value, '$[0]') = 'collar'
) WHERE routed_at IS NOT NULL OR acked_at IS NOT NULL OR resolved_by IS NOT NULL;
