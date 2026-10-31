-- Hub SQLite schema at PRAGMA user_version 3 (origin/main c27e0eac9, before
-- the memory tables). Generated with Store::open and dumped from
-- sqlite_master in creation order; the cb-memory-core upgrade test replays it.
CREATE TABLE devices (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_seen_at TEXT NOT NULL
        , kind TEXT NOT NULL DEFAULT 'human', instance_id TEXT, token_prefix TEXT);
CREATE TABLE hosts (
            id TEXT PRIMARY KEY,
            label TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            state TEXT NOT NULL,
            last_seen_at TEXT,
            node_version TEXT,
            cli_json TEXT NOT NULL DEFAULT '[]',
            capabilities_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            transport TEXT NOT NULL DEFAULT 'outbound-wss',
            labels_json TEXT NOT NULL DEFAULT '[]',
            herdr_json TEXT,
            resources_json TEXT,
            max_instances INTEGER NOT NULL DEFAULT 8,
            hostname TEXT,
            os TEXT,
            -- c-dirpicker r10 item 1: bumped by every node.hello that adopts
            -- a new link; commands stamp it at mark_forward_intent so the
            -- reconnect sweep can attribute a command to a link without any
            -- clock comparison.
            link_generation INTEGER NOT NULL DEFAULT 0
        , provider_binding TEXT NOT NULL DEFAULT 'auto', default_launch_args TEXT, claude_binary_path TEXT, default_tui TEXT, relay_bind_json TEXT, max_instances_override INTEGER, node_epoch TEXT, offline_since TEXT, token_prefix TEXT);
CREATE TABLE objects (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            media_type TEXT NOT NULL,
            stored_name TEXT NOT NULL,
            original_name TEXT,
            kind TEXT NOT NULL DEFAULT 'image',
            digest TEXT NOT NULL,
            byte_len INTEGER NOT NULL,
            bytes BLOB NOT NULL,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            anchor INTEGER
        );
CREATE UNIQUE INDEX objects_instance_digest
            ON objects(instance_id, digest);
CREATE INDEX objects_expires_at ON objects(expires_at);
CREATE TABLE ssh_hosts (
            host_id TEXT PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
            target TEXT NOT NULL UNIQUE,
            policy_json TEXT NOT NULL,
            last_error TEXT
        );
CREATE TABLE instances (
            id TEXT PRIMARY KEY,
            host_id TEXT NOT NULL,
            workspace_id TEXT,
            kind TEXT NOT NULL,
            driver TEXT NOT NULL,
            lifecycle TEXT NOT NULL,
            activity TEXT NOT NULL,
            connectivity TEXT NOT NULL,
            title TEXT,
            journal_id TEXT NOT NULL,
            durable_seq INTEGER NOT NULL DEFAULT 0,
            spec_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            last_error TEXT,
            configure_seq INTEGER NOT NULL DEFAULT 0,
            turn_error_json TEXT
        , api_route_json TEXT, mode TEXT, promoted_at TEXT, launched_by TEXT, role TEXT, scope_json TEXT, grants_json TEXT NOT NULL DEFAULT '[]', task_id TEXT, lineage_id TEXT, generation INTEGER NOT NULL DEFAULT 1, chapter_cause TEXT, fenced_at TEXT, restart_json TEXT, ended_at TEXT);
CREATE TABLE commands (
            id TEXT PRIMARY KEY,
            instance_id TEXT,
            host_id TEXT NOT NULL,
            operation TEXT NOT NULL,
            state TEXT NOT NULL,
            resolution TEXT NOT NULL,
            forwarded INTEGER NOT NULL DEFAULT 0,
            payload_json TEXT NOT NULL,
            idempotency_key TEXT UNIQUE,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            settlement_outcome TEXT,
            settlement_reason TEXT,
            settlement_http_status INTEGER,
            settlement_http_body TEXT,
            -- Generation of the host link this command's forward intent was
            -- recorded on (c-dirpicker r10 item 1); NULL for rows written
            -- before the column existed, which the reconnect sweep treats as
            -- belonging to a dead pre-upgrade link.
            link_generation INTEGER
        );
CREATE TABLE journal (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            event_id TEXT NOT NULL,
            payload_json TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            PRIMARY KEY (instance_id, seq)
        );
CREATE TABLE deleted_instances (
            instance_id TEXT PRIMARY KEY,
            deleted_at TEXT NOT NULL
        );
CREATE TABLE audit_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            device_id TEXT NOT NULL,
            action TEXT NOT NULL,
            subject TEXT,
            detail_json TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL
        );
CREATE INDEX audit_log_subject ON audit_log(subject);
CREATE TABLE fleets (
            id TEXT PRIMARY KEY,
            spec_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
CREATE TABLE fleet_members (
            fleet_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            PRIMARY KEY (fleet_id, instance_id)
        );
CREATE TABLE pair_codes (
            code_hash TEXT PRIMARY KEY,
            created_by TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used INTEGER NOT NULL DEFAULT 0
        , code_prefix TEXT, failed_attempts INTEGER NOT NULL DEFAULT 0);
CREATE TABLE enroll_tokens (
            id TEXT PRIMARY KEY,
            token_hash TEXT NOT NULL,
            token_prefix TEXT,
            created_by TEXT NOT NULL,
            expires_at TEXT NOT NULL,
            used INTEGER NOT NULL DEFAULT 0,
            used_at TEXT,
            created_at TEXT NOT NULL
        );
CREATE INDEX enroll_tokens_prefix ON enroll_tokens(token_prefix);
CREATE TABLE interactions (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            state TEXT NOT NULL,
            blocking INTEGER NOT NULL DEFAULT 1,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
CREATE INDEX interactions_instance ON interactions(instance_id);
CREATE INDEX interactions_host_state ON interactions(host_id, state);
CREATE TABLE interaction_tombstones (
            id TEXT PRIMARY KEY,
            instance_id TEXT NOT NULL,
            host_id TEXT NOT NULL,
            state TEXT NOT NULL,
            reason TEXT NOT NULL DEFAULT 'generation-ended',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
CREATE INDEX interactions_settlement_cursor
            ON interactions(state, updated_at, id);
CREATE INDEX interaction_tombstones_settlement_cursor
            ON interaction_tombstones(state, updated_at, id);
CREATE TABLE settlement_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            interaction_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'invalidated',
            reason TEXT NOT NULL DEFAULT 'generation-ended',
            created_at TEXT NOT NULL
        );
CREATE INDEX settlement_events_instance
            ON settlement_events(instance_id, seq);
CREATE INDEX settlement_events_recent
            ON settlement_events(created_at, seq);
CREATE TABLE provider_profiles (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            base_url TEXT NOT NULL DEFAULT '',
            models_json TEXT NOT NULL DEFAULT '[]',
            default_model TEXT,
            headers_json TEXT NOT NULL DEFAULT '{}',
            is_default INTEGER NOT NULL DEFAULT 0,
            revision INTEGER NOT NULL DEFAULT 1,
            secret_name TEXT,
            secret_last4 TEXT,
            secret_fingerprint TEXT,
            last_test_ok INTEGER,
            last_test_at TEXT,
            last_test_message TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        , scope TEXT NOT NULL DEFAULT 'universal', supply_json TEXT NOT NULL DEFAULT '{}', delivery_json TEXT NOT NULL DEFAULT '{}');
CREATE TABLE passkeys (
            id TEXT PRIMARY KEY,
            credential_id TEXT NOT NULL,
            public_key TEXT NOT NULL,
            counter INTEGER NOT NULL DEFAULT 0,
            transports TEXT NOT NULL DEFAULT 'null',
            name TEXT NOT NULL,
            aaguid TEXT,
            created_by TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_used_at TEXT
        );
CREATE UNIQUE INDEX passkeys_credential_id
            ON passkeys(credential_id);
CREATE TABLE project_route_overrides (
            project_id TEXT PRIMARY KEY,
            doc_json TEXT NOT NULL
        );
CREATE TABLE worktree_leases (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL,
            host_id TEXT NOT NULL,
            workspace_id TEXT NOT NULL,
            dir_key TEXT NOT NULL,
            worktree_name TEXT,
            branch TEXT,
            project_id TEXT,
            refcount INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL,
            holder_instance_id TEXT,
            task_ids_json TEXT NOT NULL DEFAULT '[]',
            base_oid TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            released_at TEXT
        );
CREATE UNIQUE INDEX worktree_leases_dir
            ON worktree_leases(host_id, workspace_id, dir_key);
CREATE INDEX worktree_leases_holder
            ON worktree_leases(holder_instance_id);
CREATE INDEX worktree_leases_state
            ON worktree_leases(host_id, workspace_id, state);
CREATE TABLE lineages (
            lineage_id TEXT PRIMARY KEY,
            current_instance_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            state TEXT NOT NULL,
            paused_by_json TEXT,
            paused_at TEXT,
            restart_json TEXT,
            origin_spec_ref TEXT,
            updated_at TEXT NOT NULL
        );
CREATE INDEX lineages_current ON lineages(current_instance_id);
CREATE UNIQUE INDEX devices_token_prefix ON devices(token_prefix) WHERE token_prefix IS NOT NULL;
CREATE UNIQUE INDEX hosts_token_prefix ON hosts(token_prefix) WHERE token_prefix IS NOT NULL;
CREATE UNIQUE INDEX pair_codes_prefix ON pair_codes(code_prefix) WHERE code_prefix IS NOT NULL;
CREATE INDEX commands_instance ON commands(instance_id, created_at DESC);
CREATE TABLE host_workspaces (
            host_id TEXT PRIMARY KEY REFERENCES hosts(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL,
            snapshot_json TEXT NOT NULL
         );
CREATE TABLE workspace_journal (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            host_id TEXT NOT NULL,
            revision INTEGER,
            command_id TEXT,
            payload_json TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            UNIQUE(host_id, revision)
         );
CREATE TABLE projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
CREATE TABLE tasks (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            parent_task_id TEXT,
            title TEXT NOT NULL,
            state TEXT NOT NULL,
            landed_sha TEXT,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         , archived_at TEXT);
CREATE INDEX tasks_project ON tasks(project_id);
CREATE INDEX tasks_parent ON tasks(parent_task_id);
CREATE TABLE task_paths (
            task_id TEXT NOT NULL,
            pattern TEXT NOT NULL,
            claimed_by TEXT NOT NULL DEFAULT '',
            claimed_at TEXT NOT NULL,
            PRIMARY KEY (task_id, pattern)
         );
CREATE INDEX task_paths_pattern ON task_paths(pattern);
CREATE TABLE placements (
            id TEXT PRIMARY KEY,
            task_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
         );
CREATE INDEX placements_task ON placements(task_id);
CREATE INDEX placements_project ON placements(project_id);
CREATE TABLE placement_ledger (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            instance_id TEXT,
            project_id TEXT,
            profile_id TEXT,
            model_id TEXT,
            host_id TEXT,
            decision_json TEXT NOT NULL,
            created_at TEXT NOT NULL
         );
CREATE INDEX placement_ledger_instance ON placement_ledger(instance_id);
CREATE INDEX placement_ledger_project ON placement_ledger(project_id);
CREATE TABLE worker_roster (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            name TEXT NOT NULL,
            host_id TEXT NOT NULL,
            state_kind TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
CREATE INDEX worker_roster_project ON worker_roster(project_id);
CREATE INDEX worker_roster_host ON worker_roster(host_id);
CREATE INDEX worker_roster_active_name
             ON worker_roster(project_id, name) WHERE state_kind != 'retired';
CREATE TABLE gate_jobs (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            state TEXT NOT NULL,
            lane_id TEXT,
            queued_at TEXT NOT NULL,
            doc_json TEXT NOT NULL,
            revision INTEGER NOT NULL DEFAULT 1,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
         );
CREATE INDEX gate_jobs_project ON gate_jobs(project_id);
CREATE INDEX gate_jobs_state ON gate_jobs(state);
CREATE INDEX gate_jobs_order ON gate_jobs(project_id, queued_at);
CREATE TABLE gate_log_objects (
            id TEXT PRIMARY KEY,
            job_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            bytes BLOB NOT NULL,
            byte_len INTEGER NOT NULL,
            digest TEXT NOT NULL,
            created_by TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
         );
CREATE INDEX gate_log_objects_job ON gate_log_objects(job_id);
CREATE INDEX gate_log_objects_expires ON gate_log_objects(expires_at);
CREATE TABLE usage_events (
            instance_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            profile_id TEXT,
            model TEXT,
            scope TEXT NOT NULL,
            scope_id TEXT,
            turn_id TEXT,
            mode TEXT NOT NULL,
            total_tokens INTEGER,
            input_tokens INTEGER,
            output_tokens INTEGER,
            cache_read_tokens INTEGER,
            cache_write_tokens INTEGER,
            cost_usd TEXT,
            accounting TEXT NOT NULL DEFAULT 'estimated',
            observed_at TEXT NOT NULL,
            observed_at_source TEXT NOT NULL DEFAULT 'ingest',
            native_context_window INTEGER,
            metric_revision INTEGER NOT NULL DEFAULT 1,
            source_session_id TEXT, lineage_state TEXT,
            PRIMARY KEY (instance_id, seq)
         );
CREATE INDEX usage_events_profile_model
             ON usage_events(profile_id, model);
CREATE INDEX usage_events_instance
             ON usage_events(instance_id);
CREATE TABLE usage_instance_era (
            instance_id TEXT PRIMARY KEY,
            cutover_seq INTEGER NOT NULL
         );
CREATE TABLE usage_schema_meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
     );
CREATE UNIQUE INDEX usage_events_turn_dedupe
             ON usage_events(
                 instance_id, scope, scope_id,
                 COALESCE(source_session_id, '')
             )
             WHERE scope = 'turn' AND scope_id IS NOT NULL;
CREATE UNIQUE INDEX usage_events_message_dedupe
             ON usage_events(instance_id, scope, scope_id)
             WHERE scope = 'message' AND scope_id IS NOT NULL;
CREATE TABLE card_tickets (
            ticket_id TEXT PRIMARY KEY,
            device_id TEXT NOT NULL,
            interaction_id TEXT NOT NULL,
            instance_id TEXT NOT NULL,
            session_key TEXT NOT NULL,
            card_message_id TEXT,
            state TEXT NOT NULL DEFAULT 'open',
            request_version INTEGER NOT NULL DEFAULT 0,
            process_generation INTEGER NOT NULL DEFAULT 0,
            request_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            expires_at_ms INTEGER NOT NULL
        );
CREATE UNIQUE INDEX card_tickets_open_interaction
            ON card_tickets(interaction_id) WHERE state = 'open';
CREATE INDEX card_tickets_device ON card_tickets(device_id);
CREATE INDEX card_tickets_session ON card_tickets(session_key);
CREATE INDEX card_tickets_state_expires
            ON card_tickets(state, expires_at_ms);
CREATE TABLE bot_owner_allowlist (
            device_id TEXT NOT NULL,
            open_id TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (device_id, open_id)
        );
PRAGMA user_version = 3;
