//! Prefixed ULIDs: `farm_01J…`, `pad_…`, `herd_…`, `ani_…`, `col_…`, `bnd_…`, `dec_…`, `mov_…`.

pub const FARM: &str = "farm";
pub const PADDOCK: &str = "pad";
pub const HERD: &str = "herd";
pub const ANIMAL: &str = "ani";
pub const COLLAR: &str = "col";
pub const BOUNDARY: &str = "bnd";
pub const DECISION: &str = "dec";
pub const EVENT: &str = "evt";
pub const MOVE: &str = "mov";

pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", ulid::Ulid::new())
}
