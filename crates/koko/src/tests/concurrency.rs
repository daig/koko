use super::*;

#[test]
fn im1_database_creation_does_not_reset_connection_state_or_snapshot() {
    let db_a = Database::in_memory();
    let a = db_a.connect();
    let a_peer = db_a.connect();
    a.query("CALL threads=1").unwrap();
    a_peer.query("CALL threads=5").unwrap();
    a.query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    a.query("BEGIN").unwrap();
    a.query("CREATE (:P {id: 1})").unwrap();

    let db_b = Database::in_memory();
    let b = db_b.connect();
    b.query("CALL threads=3").unwrap();

    assert_eq!(
        a.query("CALL current_setting('threads') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["1"]
    );
    assert_eq!(
        a_peer
            .query("CALL current_setting('threads') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["5"]
    );
    assert_eq!(
        b.query("CALL current_setting('threads') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["3"]
    );
    assert_eq!(
        a.query("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["1"]
    );
    a.query("ROLLBACK").unwrap();
}

#[test]
fn im1_context_aware_table_functions_match_standalone_calls() {
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CALL threads=3").unwrap();
    c.query("CREATE MACRO add1(x) AS x + 1").unwrap();

    let direct = c
        .query("CALL current_setting('threads') RETURN *")
        .unwrap()
        .to_result_strings();
    let filtered = c
        .query("CALL current_setting('threads') WHERE threads = '3' RETURN threads")
        .unwrap()
        .to_result_strings();
    assert_eq!(direct, vec!["3"]);
    assert_eq!(filtered, direct);

    assert_eq!(
        c.query("CALL show_macros() WHERE name = 'ADD1' RETURN name")
            .unwrap()
            .to_result_strings(),
        vec!["ADD1"]
    );
}

#[test]
fn im1_warnings_and_query_ids_are_connection_local() {
    let path = std::env::temp_dir().join(format!(
        "koko-im1-warnings-{}-{}.csv",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    std::fs::write(&path, "1\n1\n").unwrap();
    let source = path.to_string_lossy();

    let db = Database::in_memory();
    let a = db.connect();
    let b = db.connect();
    a.query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    a.query("CREATE NODE TABLE Q(id INT64, PRIMARY KEY(id))")
        .unwrap();
    b.query(&format!("COPY Q FROM \"{source}\" (IGNORE_ERRORS=true)"))
        .unwrap();
    a.query(&format!("COPY P FROM \"{source}\" (IGNORE_ERRORS=true)"))
        .unwrap();

    let a_warnings = a.query("CALL show_warnings() RETURN *").unwrap();
    let b_warnings = b.query("CALL show_warnings() RETURN *").unwrap();
    assert_eq!(a_warnings.num_rows(), 1);
    assert_eq!(b_warnings.num_rows(), 1);
    assert_eq!(a_warnings.value(0, 0).unwrap().as_int128(), Some(2));
    assert_eq!(b_warnings.value(0, 0).unwrap().as_int128(), Some(0));

    b.query("CALL clear_warnings()").unwrap();
    assert_eq!(
        b.query("CALL show_warnings() RETURN *").unwrap().num_rows(),
        0
    );
    assert_eq!(
        a.query("CALL show_warnings() WHERE query_id = 2 RETURN query_id")
            .unwrap()
            .to_result_strings(),
        vec!["2"]
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn im1_seeded_random_streams_are_connection_local() {
    let db = Database::in_memory();
    let a = db.connect();
    let b = db.connect();
    a.query("RETURN setseed(0.25)").unwrap();
    b.query("RETURN setseed(0.25)").unwrap();

    let next = |connection: &Connection| {
        connection
            .query("RETURN random()")
            .unwrap()
            .to_result_strings()
    };
    assert_eq!(next(&a), next(&b));
    let _unrelated_database = Database::in_memory();
    assert_eq!(next(&a), next(&b));
    assert_eq!(next(&a), next(&b));
}

#[test]
fn im1_runtime_settings_have_explicit_contracts() {
    let db = Database::in_memory();
    let c = db.connect();
    c.query("CALL enable_zone_map=false").unwrap();
    c.query("CALL auto_checkpoint=true").unwrap();
    let err = c.query("CALL thread=1").unwrap_err();
    assert!(err.to_string().contains("Invalid option name"), "{err}");
}
#[test]
fn im4_timeout_interrupts_statement_and_does_not_poison_connection() {
    let db = Database::in_memory();
    let connection = db.connect();
    connection.set_query_timeout_ms(1).unwrap();

    let error = connection
        .query("UNWIND range(0, 1000000) AS value RETURN sum(value)")
        .unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    assert_eq!(error.to_string(), "Interrupted.");

    connection.clear_query_timeout().unwrap();
    assert_eq!(
        connection.query("RETURN 42").unwrap().to_result_strings(),
        vec!["42"]
    );
    connection.set_query_timeout_ms(u64::MAX).unwrap();
    connection.clear_query_timeout().unwrap();
    assert!(connection.set_query_timeout(Duration::ZERO).is_err());
    let prepared = connection
        .prepare("UNWIND range(0, 1000000) AS value RETURN sum(value)")
        .unwrap();
    connection
        .set_query_timeout(Duration::from_nanos(1))
        .unwrap();
    let error = prepared.execute(&[]).unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    assert_eq!(error.to_string(), "Interrupted.");
    connection.clear_query_timeout().unwrap();
}

#[test]
fn im4_interrupt_handle_cancels_only_the_running_statement() {
    let db = Database::in_memory();
    let connection = db.connect();
    let interrupt = connection.interrupt_handle();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let interrupter = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            interrupt.interrupt();
            std::thread::yield_now();
        }
    });

    let error = connection
        .query("UNWIND range(0, 1000000) AS value RETURN sum(value)")
        .unwrap_err();
    stop.store(true, Ordering::Release);
    interrupter.join().unwrap();

    assert!(matches!(error, Error::Interrupt));
    assert_eq!(error.to_string(), "Interrupted.");
    connection.interrupt();
    assert_eq!(
        connection.query("RETURN 7").unwrap().to_result_strings(),
        vec!["7"]
    );
}

#[test]
fn im4_interrupted_mutation_rolls_back_and_releases_writer() {
    let db = Database::in_memory();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.set_query_timeout_ms(1).unwrap();

    let error = connection
        .query("UNWIND range(0, 1000000) AS id CREATE (:P {id: id})")
        .unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    connection.set_query_timeout_ms(0).unwrap();
    assert_eq!(
        connection
            .query("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["0"]
    );
    connection.query("CREATE (:P {id: 1})").unwrap();
}

#[test]
fn im4_interruption_aborts_explicit_transaction_and_releases_writer() {
    let db = Database::in_memory();
    let connection = db.connect();
    let peer = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.query("BEGIN TRANSACTION").unwrap();
    connection.set_query_timeout_ms(1).unwrap();

    let error = connection
        .query("UNWIND range(0, 1000000) AS id CREATE (:P {id: id})")
        .unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    assert_eq!(error.to_string(), "Interrupted.");
    connection.clear_query_timeout().unwrap();
    assert_eq!(
        connection
            .query("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["0"]
    );
    peer.query("CREATE (:P {id: 1})").unwrap();
}

#[test]
fn im2_database_worker_cap_constrains_connections() {
    let config = DatabaseConfig::new().with_max_workers(1).unwrap();
    let db = Database::in_memory_with_config(config).unwrap();
    let connection = db.connect();
    assert_eq!(
        connection
            .query("CALL current_setting('threads') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["1"]
    );
    assert!(connection.set_max_num_threads(0).is_err());
    assert!(connection.set_max_num_threads(2).is_err());
    connection.set_max_num_threads(1).unwrap();
    let error = connection.query("CALL threads=2").unwrap_err();
    assert!(
        error.to_string().contains("max_workers") && error.to_string().contains('1'),
        "{error}"
    );
}

#[test]
fn im2_optimizer_and_worker_counts_preserve_results() {
    let db = Database::in_memory();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, group_id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .query(&format!(
            "UNWIND range(0, {}) AS id \
             CREATE (:P {{id: id, group_id: id % 17}})",
            VECTOR_CAPACITY
        ))
        .unwrap();
    let query = "MATCH (p:P) WHERE p.id % 3 = 0 \
         RETURN p.group_id, count(*) ORDER BY p.group_id";

    connection.set_max_num_threads(1).unwrap();
    let serial = connection.query(query).unwrap().to_result_strings();
    connection.set_max_num_threads(4).unwrap();
    let parallel = connection.query(query).unwrap().to_result_strings();
    assert_eq!(parallel, serial);

    connection
        .query("CALL enable_plan_optimizer=false")
        .unwrap();
    let unoptimized = connection.query(query).unwrap().to_result_strings();
    assert_eq!(unoptimized, serial);
}

#[test]
fn im2_batch_writes_cross_vector_boundaries() {
    let db = Database::in_memory();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, value INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .query("CREATE REL TABLE R(FROM P TO P, weight INT64)")
        .unwrap();
    connection
        .query(&format!(
            "UNWIND range(0, {}) AS id CREATE (:P {{id: id, value: id}})",
            VECTOR_CAPACITY
        ))
        .unwrap();
    connection
        .query("MATCH (p:P) CREATE (p)-[:R {weight: p.id}]->(p)")
        .unwrap();

    let rows = (VECTOR_CAPACITY + 1) as i64;
    let sum = rows * (rows - 1) / 2;
    assert_eq!(
        connection
            .query("MATCH ()-[r:R]->() RETURN count(*), sum(r.weight)")
            .unwrap()
            .to_result_strings(),
        vec![format!("{rows}|{sum}")]
    );

    connection
        .query("MATCH (p:P) SET p.value = p.id + 1")
        .unwrap();
    assert_eq!(
        connection
            .query("MATCH (p:P) RETURN sum(p.value)")
            .unwrap()
            .to_result_strings(),
        vec![(sum + rows).to_string()]
    );

    connection.query("MATCH ()-[r:R]->() DELETE r").unwrap();
    connection
        .query("MATCH (p:P) WHERE p.id % 2 = 0 DELETE p")
        .unwrap();
    assert_eq!(
        connection
            .query("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec![(VECTOR_CAPACITY / 2).to_string()]
    );
}

#[test]
fn im2_stats_info_is_per_property_and_snapshot_correct() {
    let db = Database::in_memory();
    let writer = db.connect();
    let reader = db.connect();
    writer
        .query(
            "CREATE NODE TABLE P(\
             id INT64, gender STRING, tags INT64[], PRIMARY KEY(id))",
        )
        .unwrap();
    writer
        .query(
            "CREATE (:P {id: 1, gender: 'f', tags: [1]}), \
                     (:P {id: 2, gender: 'm', tags: [2]}), \
                     (:P {id: 3, gender: 'f', tags: [1]})",
        )
        .unwrap();

    let committed = writer.query("CALL stats_info('P') RETURN *").unwrap();
    assert_eq!(
        committed.column_names(),
        &[
            "cardinality",
            "id_distinct_count",
            "gender_distinct_count",
            "tags_distinct_count",
        ]
    );
    assert_eq!(
        committed
            .schema()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect::<Vec<_>>(),
        vec![
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
            LogicalType::Int64,
        ]
    );
    assert_eq!(committed.to_result_strings(), vec!["3|3|2|0"]);

    let transaction = writer.transaction().unwrap();
    transaction
        .query("CREATE (:P {id: 4, gender: 'x', tags: [4]})")
        .unwrap();
    assert_eq!(
        transaction
            .query("CALL stats_info('P') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["4|4|3|0"]
    );
    assert_eq!(
        reader
            .query("CALL stats_info('P') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["3|3|2|0"]
    );
    transaction.rollback().unwrap();
    assert_eq!(
        writer
            .query("CALL stats_info('P') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["3|3|2|0"]
    );

    writer.query("MATCH (p:P) WHERE p.id = 3 DELETE p").unwrap();
    assert_eq!(
        writer
            .query("CALL stats_info('P') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["2|2|2|0"]
    );
    writer.query("MATCH (p:P) SET p.gender = 'x'").unwrap();
    assert_eq!(
        writer
            .query("CALL stats_info('P') RETURN *")
            .unwrap()
            .to_result_strings(),
        vec!["2|2|1|0"]
    );

    writer
        .query("CREATE REL TABLE R(FROM P TO P, weight INT64)")
        .unwrap();
    let error = writer.query("CALL stats_info('R') RETURN *").unwrap_err();
    assert!(error.to_string().contains("non-node table R"), "{error}");
}

#[derive(Debug, Default)]
struct RecordingMemoryResource {
    current: AtomicU64,
    peak: AtomicU64,
}

impl MemoryResource for RecordingMemoryResource {
    fn try_reserve(&self, bytes: u64) -> Result<()> {
        let current = self.current.fetch_add(bytes, Ordering::AcqRel) + bytes;
        self.peak.fetch_max(current, Ordering::AcqRel);
        Ok(())
    }

    fn release(&self, bytes: u64) {
        self.current.fetch_sub(bytes, Ordering::AcqRel);
    }
}

#[test]
fn im2_application_memory_resource_observes_database_allocations() {
    let resource = Arc::new(RecordingMemoryResource::default());
    let config = DatabaseConfig::new().with_memory_resource(resource.clone());
    assert!(config.memory_resource().is_some());
    let db = Database::in_memory_with_config(config).unwrap();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, payload STRING, PRIMARY KEY(id))")
        .unwrap();
    connection
        .query("CREATE (:P {id: 1, payload: 'tracked'})")
        .unwrap();

    let usage = db.memory_usage();
    assert!(usage.current > 0);
    assert_eq!(resource.current.load(Ordering::Acquire), usage.current);
    assert_eq!(resource.peak.load(Ordering::Acquire), usage.peak);

    drop(connection);
    drop(db);
    assert_eq!(resource.current.load(Ordering::Acquire), 0);
}

#[derive(Debug, Default)]
struct DenyingMemoryResource {
    attempts: AtomicU64,
}

impl MemoryResource for DenyingMemoryResource {
    fn try_reserve(&self, _bytes: u64) -> Result<()> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        Err(Error::buffer_manager())
    }

    fn release(&self, _bytes: u64) {
        panic!("a rejected application reservation must not be released");
    }
}

#[test]
fn im4_application_memory_resource_can_reject_without_accounting_drift() {
    let resource = Arc::new(DenyingMemoryResource::default());
    let db = Database::in_memory_with_config(
        DatabaseConfig::new().with_memory_resource(resource.clone()),
    )
    .unwrap();

    let error = db.connect().query("RETURN 1").unwrap_err();
    assert!(matches!(error, Error::BufferManager));
    assert_eq!(
        error.to_string(),
        "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
    );
    assert!(resource.attempts.load(Ordering::Acquire) > 0);
    assert_eq!(db.memory_usage(), MemoryUsage::default());
}

#[test]
fn im2_memory_limit_is_enforced_and_reported_per_database() {
    let fixed_bytes = koko_common::ColumnData::allocation_bytes(LogicalType::Int64.physical_type())
        + koko_common::ColumnData::allocation_bytes(LogicalType::String.physical_type());
    let result_allowance =
        2 * koko_common::ColumnData::allocation_bytes(LogicalType::String.physical_type()) + 8192;
    let memory_limit = fixed_bytes + result_allowance;
    let config = DatabaseConfig::new()
        .with_memory_limit(memory_limit)
        .unwrap();
    let db = Database::in_memory_with_config(config).unwrap();
    let other = Database::in_memory();
    let c = db.connect();
    c.query("CREATE NODE TABLE P(id INT64, payload STRING, PRIMARY KEY(id))")
        .unwrap();

    let oversized = "x".repeat((result_allowance + 4096) as usize);
    let err = c
        .query(&format!("CREATE (:P {{id: 1, payload: '{oversized}'}})"))
        .unwrap_err();
    assert!(matches!(err, Error::BufferManager));
    assert_eq!(
        err.to_string(),
        "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
    );
    assert_eq!(
        c.query("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["0"]
    );
    assert_eq!(db.memory_usage().current, 0);
    assert_eq!(other.memory_usage().current, 0);

    c.query("CREATE (:P {id: 2, payload: 'small'})").unwrap();
    let usage = db.memory_usage();
    assert!(usage.current > 0);
    assert_eq!(usage.limit, Some(memory_limit));
    let bm = c.query("CALL bm_info() RETURN *").unwrap();
    assert_eq!(
        bm.value(0, 0).unwrap().as_u128(),
        Some(memory_limit as u128)
    );
    assert_eq!(
        bm.value(0, 1).unwrap().as_u128(),
        Some(usage.current as u128)
    );
}

#[test]
fn im4_repeated_hash_aggregate_exhaustion_is_catchable_and_releases_memory() {
    let db = Database::in_memory_with_config(
        DatabaseConfig::new().with_memory_limit(64 * 1024).unwrap(),
    )
    .unwrap();
    let connection = db.connect();

    for _ in 0..2 {
        let error = connection
            .query("UNWIND range(0, 100000) AS value RETURN value, count(*)")
            .unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(db.memory_usage().current, 0);
    }
    assert_eq!(
        connection.query("RETURN 1").unwrap().to_result_strings(),
        vec!["1"]
    );
}

#[test]
fn im4_correlated_subplans_reuse_temporary_memory_budget() {
    let db = Database::in_memory_with_config(
        DatabaseConfig::new()
            .with_memory_limit(4 * 1024 * 1024)
            .unwrap(),
    )
    .unwrap();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection
        .query("CREATE NODE TABLE C(id INT64, PRIMARY KEY(id))")
        .unwrap();
    connection.query("CREATE REL TABLE R(FROM P TO C)").unwrap();
    connection
        .query("UNWIND range(0, 99) AS id CREATE (:P {id: id}), (:C {id: id})")
        .unwrap();
    connection
        .query(
            "UNWIND range(0, 99) AS id \
             MATCH (p:P), (c:C) WHERE p.id = id AND c.id = id \
             CREATE (p)-[:R]->(c)",
        )
        .unwrap();
    let storage_bytes = db.memory_usage().current;

    let result = connection
        .query(
            "MATCH (:P)-[:R]->(c:C) \
             WHERE NOT EXISTS { \
               MATCH (:P)-[:R]->(inner:C) \
               WHERE inner.id = -1 AND c.id >= 0 \
             } \
             RETURN count(*)",
        )
        .unwrap();
    assert_eq!(result.to_result_strings(), vec!["100"]);
    drop(result);
    assert_eq!(db.memory_usage().current, storage_bytes);
}

#[test]
fn im4_copy_ignore_errors_never_swallows_memory_exhaustion() {
    let fixed_bytes = koko_common::ColumnData::allocation_bytes(LogicalType::Int64.physical_type())
        + koko_common::ColumnData::allocation_bytes(LogicalType::String.physical_type());
    let db = Database::in_memory_with_config(
        DatabaseConfig::new()
            .with_memory_limit(fixed_bytes + 1024)
            .unwrap(),
    )
    .unwrap();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, payload STRING, PRIMARY KEY(id))")
        .unwrap();
    let path = std::env::temp_dir().join(format!(
        "koko-im4-oom-{}-{}.csv",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    std::fs::write(&path, format!("1,{}\n", "x".repeat(4096))).unwrap();

    for _ in 0..2 {
        let error = connection
            .query(&format!(
                "COPY P FROM \"{}\" (HEADER=false, IGNORE_ERRORS=true)",
                path.to_string_lossy()
            ))
            .unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(db.memory_usage().current, 0);
        assert_eq!(
            connection
                .query("MATCH (p:P) RETURN count(*)")
                .unwrap()
                .to_result_strings(),
            vec!["0"]
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn im4_interrupt_cancels_copy_and_rolls_back_partial_batches() {
    let db = Database::in_memory();
    let connection = db.connect();
    connection
        .query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let path = std::env::temp_dir().join(format!(
        "koko-im4-interrupt-copy-{}-{}.csv",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let records = (0..200_000).map(|id| format!("{id}\n")).collect::<String>();
    std::fs::write(&path, records).unwrap();

    let interrupt = connection.interrupt_handle();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let interrupter = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Acquire) {
            interrupt.interrupt();
            std::thread::yield_now();
        }
    });
    let result = connection.query(&format!("COPY P FROM '{}' (HEADER=false)", path.display()));
    stop.store(true, Ordering::Release);
    interrupter.join().unwrap();
    std::fs::remove_file(path).unwrap();

    let error = result.unwrap_err();
    assert!(matches!(error, Error::Interrupt));
    assert_eq!(error.to_string(), "Interrupted.");
    assert_eq!(
        connection
            .query("MATCH (p:P) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["0"]
    );
}

#[test]
fn im2_holding_and_dropping_results_updates_memory_usage() {
    let db = Database::in_memory();
    let connection = db.connect();
    let baseline = db.memory_usage().current;
    let result = connection
        .query(&format!(
            "UNWIND range(0, {}) AS value RETURN value",
            VECTOR_CAPACITY
        ))
        .unwrap();
    assert!(db.memory_usage().current > baseline);
    drop(result);
    assert_eq!(db.memory_usage().current, baseline);

    let one_chunk =
        koko_common::ColumnData::allocation_bytes(LogicalType::Int64.physical_type()) + 4096;
    let limited = Database::in_memory_with_config(
        DatabaseConfig::new().with_memory_limit(one_chunk).unwrap(),
    )
    .unwrap();
    let error = limited
        .connect()
        .query(&format!(
            "UNWIND range(0, {}) AS value RETURN value",
            VECTOR_CAPACITY
        ))
        .unwrap_err();
    assert!(matches!(error, Error::BufferManager));
    assert_eq!(
        error.to_string(),
        "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
    );
    assert_eq!(limited.memory_usage().current, 0);
}

#[test]
fn im2_rollback_releases_tail_row_reservations() {
    let config = DatabaseConfig::new()
        .with_memory_limit(1024 * 1024)
        .unwrap();
    let db = Database::in_memory_with_config(config).unwrap();
    let c = db.connect();
    c.query("CREATE NODE TABLE P(id INT64, payload STRING, PRIMARY KEY(id))")
        .unwrap();
    c.query("CREATE (:P {id: 1, payload: 'committed'})")
        .unwrap();
    let committed = db.memory_usage().current;

    c.query("BEGIN").unwrap();
    c.query("CREATE (:P {id: 2, payload: 'rolled back'})")
        .unwrap();
    assert!(db.memory_usage().current > committed);
    c.query("ROLLBACK").unwrap();
    assert_eq!(db.memory_usage().current, committed);
}

#[test]
fn im3_load_uses_home_and_ordered_search_path_settings() {
    let root = std::env::temp_dir().join(format!(
        "koko-im3-resolver-{}-{}",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    let home = root.join("home");
    let search_a = root.join("search-a");
    let search_b = root.join("search-b");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&search_a).unwrap();
    std::fs::create_dir_all(&search_b).unwrap();
    std::fs::write(home.join("home.csv"), "id\n7\n").unwrap();
    std::fs::write(search_a.join("ordered.csv"), "1\n2\n").unwrap();
    std::fs::write(search_b.join("ordered.csv"), "3\n4\n").unwrap();

    let connection = Database::in_memory().connect();
    connection
        .query(&format!("CALL home_directory='{}'", home.display()))
        .unwrap();
    assert_eq!(
        connection
            .query("LOAD FROM '~/home.csv' RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["1"]
    );
    connection
        .query(&format!(
            "CALL file_search_path='{},{}'",
            search_a.display(),
            search_b.display()
        ))
        .unwrap();
    assert_eq!(
        connection
            .query("LOAD FROM 'ordered.csv' RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["4"]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn im3_copy_preflights_all_files_and_orders_limited_warnings() {
    let root = std::env::temp_dir().join(format!(
        "koko-im3-copy-{}-{}",
        std::process::id(),
        std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let good = root.join("good.csv");
    let bad_arity = root.join("bad.csv");
    let warnings = root.join("warnings.csv");
    std::fs::write(&good, "1\n").unwrap();
    std::fs::write(&bad_arity, "2,extra\n").unwrap();
    std::fs::write(&warnings, "id\n1\nbad-a\n2\nbad-b\n3\nbad-c\n").unwrap();

    let connection = Database::in_memory().connect();
    connection
        .query("CREATE NODE TABLE Item(id INT64, PRIMARY KEY(id))")
        .unwrap();
    let error = connection
        .query(&format!(
            "COPY Item FROM ['{}', '{}']",
            good.display(),
            bad_arity.display()
        ))
        .unwrap_err();
    assert!(
        error.to_string().contains("Number of columns mismatch"),
        "{error}"
    );
    assert_eq!(
        connection
            .query("MATCH (n:Item) RETURN count(*)")
            .unwrap()
            .to_result_strings(),
        vec!["0"]
    );

    connection.query("CALL warning_limit=2").unwrap();
    let result = connection
        .query(&format!(
            "COPY Item FROM '{}' (HEADER=true, IGNORE_ERRORS=true)",
            warnings.display()
        ))
        .unwrap();
    assert_eq!(result.num_rows(), 2);
    let shown = connection
        .query(
            "CALL show_warnings() RETURN line_number, skipped_line_or_record ORDER BY line_number",
        )
        .unwrap()
        .to_result_strings();
    assert_eq!(shown, vec!["3|bad-a", "5|bad-b"]);
    std::fs::remove_dir_all(root).unwrap();
}
