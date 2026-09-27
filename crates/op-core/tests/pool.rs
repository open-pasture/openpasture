//! The SQLite pool is as large as `OPENPASTURE_DB_POOL` asks (1 to 256), 16 by default.

use op_core::store::{DEFAULT_POOL, POOL_ENV, pool_size};

#[test]
fn pool_size_reads_the_setting_and_falls_back_to_sixteen() {
    assert_eq!(DEFAULT_POOL, 16);
    assert_eq!(pool_size(None), 16);
    assert_eq!(pool_size(Some("32")), 32);
    assert_eq!(pool_size(Some(" 4 ")), 4);
    for bad in ["", "0", "-3", "257", "lots", "8.5"] {
        assert_eq!(pool_size(Some(bad)), 16, "{bad:?}");
    }
}

#[tokio::test]
async fn a_store_opens_the_pool_it_is_given() {
    let dir = tempfile::tempdir().unwrap();
    let store = op_core::Store::open(dir.path()).await.unwrap();
    let want = pool_size(std::env::var(POOL_ENV).ok().as_deref());
    assert_eq!(store.pool().options().get_max_connections(), want);
}
