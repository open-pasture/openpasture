-- op-alerts (hosted relay): a text to the relay's number is routed by the
-- keys that proved the number it came from, so recipients are also looked up
-- by address.
CREATE INDEX notify_recipients_by_address ON notify_recipients(channel, address);
