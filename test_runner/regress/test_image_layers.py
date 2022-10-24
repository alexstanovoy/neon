import os
import shutil
from contextlib import closing
from datetime import datetime
from pathlib import Path
from typing import List

import pytest
from fixtures.neon_fixtures import NeonEnv, wait_for_last_flush_lsn


def test_image_layer_fail_before_finish(neon_simple_env: NeonEnv):
    pageserver_http = neon_simple_env.pageserver.http_client()
    pageserver_http.configure_failpoints(("image-layer-fail-before-finish", "return"))

    image_files = []

    with pytest.raises(Exception):
        tenant_id, _ = neon_simple_env.neon_cli.create_tenant(
            conf={
                "gc_period": "10 m",
                "gc_horizon": "1048576",
                "checkpoint_distance": "4194304",
                "compaction_period": "10 m",
                "compaction_threshold": "2",
                "compaction_target_size": "4194304",
            }
        )

        new_timeline_id = neon_simple_env.neon_cli.create_branch("test_image_layer_fail_before_finish", )
        pg = neon_simple_env.postgres.create_start("test_image_layer_fail_before_finish")
        pg.safe_psql_many(
            [
                "CREATE TABLE foo (t text) WITH (autovacuum_enabled = off)",
                """INSERT INTO foo
            SELECT 'long string to consume some space' || g
            FROM generate_series(1, 100000) g""",
            ]
        )
        wait_for_last_flush_lsn(neon_simple_env, pg, neon_simple_env.initial_tenant, new_timeline_id)
        pageserver_http.timeline_compact(tenant_id, new_timeline_id)

        image_files = [path for path in neon_simple_env.timeline_dir(tenant_id, new_timeline_id).iterdir()]

    assert (
        len(image_files) == 0
    ), "pageserver should clean its temp new image layer files on failure"
