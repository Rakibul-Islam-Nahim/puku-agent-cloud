-- Why a machine is not running, as a word a client can act on
-- (`capacity_full`, `image_not_staged`, ...; docs/MACHINES-API.md, "When a
-- machine cannot start"). `error` keeps the human sentence.
ALTER TABLE machines ADD COLUMN last_reason text;
