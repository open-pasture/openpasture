-- Every collar is a device; the sim/device kind was never read.
ALTER TABLE collars DROP COLUMN kind;
