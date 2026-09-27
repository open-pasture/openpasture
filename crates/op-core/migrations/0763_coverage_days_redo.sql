-- op-analytics (H): days aggregated before 0762 have no fix rate or cell
-- signal. Forgetting what the day aggregator has read makes its next run
-- redo every day there is data for, the new columns included.
DELETE FROM analytics_day_marks;
DELETE FROM analytics_days;
