//! Opt-in proof that a dry run writes nothing (docs/07 §13).
//!
//! The request below is deliberately the destructive one — `dropTargetBeforeCreate`
//! against a target that already holds one of the selected tables — because that is the
//! request a dry run must refuse to act on. It drives the real `run_transfer` with
//! `dry_run: true`, then compares the target's table list, every `SHOW CREATE TABLE` and
//! every row count against a snapshot taken beforehand; a backup table leaked by the
//! rename pre-pass would surface as a new table name in that same comparison. It then
//! asserts the plan is real (it holds the create/rename/cleanup statements), and finally
//! replays the identical request with `dry_run: false`: "nothing changed" is only
//! meaningful if something *would* have changed.
//!
//! Two things are out of reach here and stay covered elsewhere: the plugin gate
//! (`dbx-plugin-runtime::transfer::gate`, unit-tested) and the three checks a dry run is
//! allowed to skip — read-only target, drop confirmation, task journal registration. Those
//! live in `src-tauri::commands::transfer::launch_transfer`, which needs an `AppHandle`.
//!
//! Requires a disposable MySQL 8 via `DBX_LIVE_MYSQL_TRANSFER_HOST/PORT/USER/PASSWORD`.
//! Creates UUID-named databases and drops only those.

use dbx_core::connection::AppState;
use dbx_core::db::mysql;
use dbx_core::models::connection::{ConnectionConfig, DatabaseType};
use dbx_core::storage::Storage;
use dbx_core::transfer::{TransferProgress, TransferRequest, TransferStatus};
use dbx_core::transfer_rebuild::BACKUP_TABLE_MARKER;
use futures::FutureExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const CONNECTION_ID: &str = "dry-run";

fn live_mysql_config() -> ConnectionConfig {
    serde_json::from_value(json!({
        "id": CONNECTION_ID, "name": CONNECTION_ID, "db_type": "mysql",
        "host": std::env::var("DBX_LIVE_MYSQL_TRANSFER_HOST").expect("DBX_LIVE_MYSQL_TRANSFER_HOST"),
        "port": std::env::var("DBX_LIVE_MYSQL_TRANSFER_PORT").ok().and_then(|s| s.parse::<u16>().ok()).unwrap_or(3306),
        "username": std::env::var("DBX_LIVE_MYSQL_TRANSFER_USER").unwrap_or_else(|_| "root".into()),
        "password": std::env::var("DBX_LIVE_MYSQL_TRANSFER_PASSWORD").expect("DBX_LIVE_MYSQL_TRANSFER_PASSWORD"),
        "connect_timeout_secs": 10, "query_timeout_secs": 30, "keepalive_interval_secs": 0
    }))
    .unwrap()
}

async fn query_rows(pool: &mysql::MySqlPool, sql: &str) -> Result<Vec<Vec<serde_json::Value>>, String> {
    Ok(mysql::execute_query(pool, sql, false).await?.rows)
}

async fn query_text(pool: &mysql::MySqlPool, sql: &str) -> Result<String, String> {
    let rows = query_rows(pool, sql).await?;
    Ok(rows.first().and_then(|row| row.first()).and_then(|cell| cell.as_str()).unwrap_or_default().to_string())
}

async fn table_names(pool: &mysql::MySqlPool, database: &str) -> Result<Vec<String>, String> {
    let rows = query_rows(
        pool,
        &format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = '{database}' ORDER BY table_name"
        ),
    )
    .await?;
    rows.iter()
        .map(|row| row[0].as_str().map(str::to_string).ok_or_else(|| "table_name should be a string".to_string()))
        .collect()
}

async fn row_count(pool: &mysql::MySqlPool, database: &str, table: &str) -> Result<u64, String> {
    let count = query_text(pool, &format!("SELECT CAST(COUNT(*) AS CHAR) FROM `{database}`.`{table}`")).await?;
    count.parse::<u64>().map_err(|error| format!("COUNT(*) for {table} should parse: {error}"))
}

/// Everything a write could change about one database: which tables exist, the DDL of
/// each, and how many rows each holds.
async fn fingerprint(pool: &mysql::MySqlPool, database: &str) -> Result<String, String> {
    let mut snapshot = String::new();
    for table in table_names(pool, database).await? {
        let ddl = query_rows(pool, &format!("SHOW CREATE TABLE `{database}`.`{table}`")).await?;
        let ddl = ddl.first().and_then(|row| row.get(1)).and_then(|cell| cell.as_str()).unwrap_or_default();
        snapshot.push_str(&format!("{table}\n{ddl}\nrows={}\n--\n", row_count(pool, database, &table).await?));
    }
    Ok(snapshot)
}

#[test]
#[ignore = "requires a disposable MySQL 8 via DBX_LIVE_MYSQL_TRANSFER_HOST/PORT/USER/PASSWORD"]
fn live_mysql_dry_run_leaves_the_target_untouched() {
    // The unoptimized driver/setup futures exceed the default test-thread stack.
    std::thread::Builder::new()
        .name("mysql-dry-run".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_live_mysql_dry_run_leaves_the_target_untouched());
        })
        .unwrap()
        .join()
        .unwrap();
}

async fn run_live_mysql_dry_run_leaves_the_target_untouched() {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let source_db = format!("dbx_dryrun_src_{}", &suffix[..12]);
    let target_db = format!("dbx_dryrun_dst_{}", &suffix[..12]);
    let config = live_mysql_config();
    let url = format!("mysql://{}:{}@{}:{}", config.username, config.password, config.host, config.port);
    let pool = mysql::connect(&url, Duration::from_secs(10)).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let storage = Storage::open(&directory.path().join("state.db")).await.unwrap();
    let state = Arc::new(AppState::new_with_plugin_and_agent_dir_and_app_version(
        storage,
        directory.path().join("plugins"),
        directory.path().join("agents"),
        env!("CARGO_PKG_VERSION"),
    ));
    state.configs.write().await.insert(config.id.clone(), config);

    let outcome = std::panic::AssertUnwindSafe(async {
        // Source: a parent/child pair with a foreign key, so the create pass has real
        // dependency ordering to resolve. Target: `parent` already exists with a different
        // shape and its own two rows - the table a rebuild would rename to a backup.
        mysql::execute_query(
            &pool,
            &format!(
                "CREATE DATABASE `{source_db}`; CREATE DATABASE `{target_db}`; \
                 CREATE TABLE `{source_db}`.`parent` (\
                   id INT NOT NULL, name VARCHAR(50), PRIMARY KEY (id)) ENGINE=InnoDB; \
                 CREATE TABLE `{source_db}`.`child` (\
                   id INT NOT NULL, parent_id INT NOT NULL, amount DECIMAL(10,2), PRIMARY KEY (id), \
                   KEY idx_parent (parent_id), \
                   CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) REFERENCES `{source_db}`.`parent` (id)\
                 ) ENGINE=InnoDB; \
                 INSERT INTO `{source_db}`.`parent` VALUES (1, 'a'), (2, 'b'), (3, 'c'); \
                 INSERT INTO `{source_db}`.`child` VALUES (1, 1, 10.50), (2, 2, 20.50); \
                 CREATE TABLE `{target_db}`.`parent` (\
                   id INT NOT NULL, name VARCHAR(50), notes VARCHAR(20), PRIMARY KEY (id)) ENGINE=InnoDB; \
                 INSERT INTO `{target_db}`.`parent` VALUES (98, 'left alone', 'keep'), (99, 'left alone 2', 'keep')"
            ),
            true,
        )
        .await?;

        let source = state.get_or_create_pool(CONNECTION_ID, Some(&source_db)).await?;
        let target = state.get_or_create_pool(CONNECTION_ID, Some(&target_db)).await?;
        // `dropTargetConfirmed` is what the user's approval would have set; the host's
        // gate checks it before `run_transfer`, so setting it here keeps this test about
        // the write path and nothing else.
        let request: TransferRequest = serde_json::from_value(json!({
            "transferId": format!("dry-run-{suffix}"),
            "sourceConnectionId": CONNECTION_ID, "sourceDatabase": source_db, "sourceSchema": source_db,
            "targetConnectionId": CONNECTION_ID, "targetDatabase": target_db, "targetSchema": target_db,
            "tables": ["parent", "child"],
            "createTable": true, "dropTargetBeforeCreate": true, "dropTargetConfirmed": true,
            "content": "structureAndData", "mode": "append", "batchSize": 2
        }))
        .unwrap();

        // The plan is real and would create two tables, rename one of them to a backup and
        // copy five rows; this is the state a dry run must leave untouched.
        let tables_before = table_names(&pool, &target_db).await?;
        assert_eq!(tables_before, vec!["parent".to_string()]);
        let before = fingerprint(&pool, &target_db).await?;

        let mut events: Vec<TransferProgress> = Vec::new();
        let terminal = dbx_core::transfer_runner::run_transfer(
            state.clone(),
            request.clone(),
            DatabaseType::Mysql,
            DatabaseType::Mysql,
            source.clone(),
            target.clone(),
            // No journal: a dry run is launched with `None` so it cannot register a
            // history row for work it never did.
            None,
            None,
            true,
            |event| events.push(event),
        )
        .await;

        assert!(matches!(terminal.status, TransferStatus::Done), "{terminal:?}");
        assert_eq!(terminal.table, "dry-run");
        assert_eq!(terminal.total_tables, 2, "the plan resolved both selected tables");
        assert_eq!(terminal.rows_transferred, 0);
        assert!(terminal.error.is_none(), "{terminal:?}");
        // It stopped before the first write point, so it had nothing to report per table.
        assert!(events.is_empty(), "{events:?}");

        assert_eq!(table_names(&pool, &target_db).await?, tables_before, "a dry run must not add a table");
        assert_eq!(fingerprint(&pool, &target_db).await?, before, "a dry run must not change DDL or rows");

        // Skipping the writes must not skip the planning: the returned products are the
        // statements the real run would execute.
        let products = dbx_core::plugin_transfer::dry_run_products(&state, &request).await?;
        for table in ["parent", "child"] {
            assert!(
                products.operations.iter().any(|operation| operation.target_table.as_deref() == Some(table)),
                "{table} is missing from {:?}",
                products.operations
            );
            assert!(products.structure_sql.contains(table), "{table} is missing from the planned DDL");
        }
        assert!(products.structure_sql.to_uppercase().contains("CREATE TABLE"), "{}", products.structure_sql);
        let rebuild_sql = products.rebuild_sql.as_deref().expect("a rebuild plan");
        assert!(rebuild_sql.to_uppercase().contains("RENAME TABLE"), "{rebuild_sql}");
        assert!(rebuild_sql.contains(BACKUP_TABLE_MARKER), "{rebuild_sql}");
        let cleanup_sql = products.cleanup_sql.as_deref().expect("a backup cleanup plan");
        assert!(cleanup_sql.to_uppercase().contains("DROP TABLE"), "{cleanup_sql}");

        // The counter-check: the identical request, actually executed. Without this the
        // assertions above would also pass for a request that never writes anything.
        let mut real_events: Vec<TransferProgress> = Vec::new();
        let real = dbx_core::transfer_runner::run_transfer(
            state.clone(),
            request.clone(),
            DatabaseType::Mysql,
            DatabaseType::Mysql,
            source,
            target,
            None,
            None,
            false,
            |event| real_events.push(event),
        )
        .await;
        assert!(matches!(real.status, TransferStatus::Done), "{real:?}");
        assert_eq!(table_names(&pool, &target_db).await?, vec!["child".to_string(), "parent".to_string()]);
        assert_eq!(row_count(&pool, &target_db, "parent").await?, 3, "the rebuild copied the source rows");
        assert_eq!(row_count(&pool, &target_db, "child").await?, 2);
        assert_ne!(
            fingerprint(&pool, &target_db).await?,
            before,
            "the real run must change what the dry run left alone"
        );
        Ok::<(), String>(())
    })
    .catch_unwind()
    .await;

    // Only this test's UUID-named databases are ever dropped.
    let cleanup = mysql::execute_query(
        &pool,
        &format!("DROP DATABASE IF EXISTS `{source_db}`; DROP DATABASE IF EXISTS `{target_db}`"),
        true,
    )
    .await;
    pool.disconnect().await.unwrap();
    outcome.unwrap().unwrap();
    cleanup.unwrap();
}
