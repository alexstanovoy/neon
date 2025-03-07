from fixtures.log_helper import log
from fixtures.neon_fixtures import NeonEnvBuilder, wait_until
from fixtures.types import TenantId, TimelineId


def get_only_element(l):  # noqa: E741
    assert len(l) == 1
    return l[0]


# Test that gc and compaction tenant tasks start and stop correctly
def test_tenant_tasks(neon_env_builder: NeonEnvBuilder):
    # The gc and compaction loops don't bother to watch for tenant state
    # changes while sleeping, so we use small periods to make this test
    # run faster. With default settings we'd have to wait longer for tasks
    # to notice state changes and shut down.
    # TODO fix this behavior in the pageserver
    tenant_config = "{gc_period = '1 s', compaction_period = '1 s'}"
    neon_env_builder.pageserver_config_override = f"tenant_config={tenant_config}"
    name = "test_tenant_tasks"
    env = neon_env_builder.init_start()
    client = env.pageserver.http_client()

    def get_state(tenant):
        all_states = client.tenant_list()
        matching = [t for t in all_states if TenantId(t["id"]) == tenant]
        return get_only_element(matching)["state"]

    def get_metric_value(name):
        metrics = client.get_metrics()
        relevant = [line for line in metrics.splitlines() if line.startswith(name)]
        if len(relevant) == 0:
            return 0
        line = get_only_element(relevant)
        value = line.lstrip(name).strip()
        return int(value)

    def delete_all_timelines(tenant: TenantId):
        timelines = [TimelineId(t["timeline_id"]) for t in client.timeline_list(tenant)]
        for t in timelines:
            client.timeline_delete(tenant, t)

    def assert_active_without_jobs(tenant):
        assert get_state(tenant) == {"Active": {"background_jobs_running": False}}

    # Create tenant, start compute
    tenant, _ = env.neon_cli.create_tenant()
    env.neon_cli.create_timeline(name, tenant_id=tenant)
    pg = env.postgres.create_start(name, tenant_id=tenant)
    assert get_state(tenant) == {
        "Active": {"background_jobs_running": True}
    }, "Pageserver should activate a tenant and start background jobs if timelines are loaded"

    # Stop compute
    pg.stop()

    # Delete all timelines on all tenants
    for tenant_info in client.tenant_list():
        tenant_id = TenantId(tenant_info["id"])
        delete_all_timelines(tenant_id)
        wait_until(10, 0.2, lambda: assert_active_without_jobs(tenant_id))

    # Assert that all tasks finish quickly after tenant is detached
    assert get_metric_value('pageserver_tenant_task_events{event="start"}') > 0
    client.tenant_detach(tenant)
    client.tenant_detach(env.initial_tenant)

    def assert_tasks_finish():
        tasks_started = get_metric_value('pageserver_tenant_task_events{event="start"}')
        tasks_ended = get_metric_value('pageserver_tenant_task_events{event="stop"}')
        tasks_panicked = get_metric_value('pageserver_tenant_task_events{event="panic"}')
        log.info(f"started {tasks_started}, ended {tasks_ended}, panicked {tasks_panicked}")
        assert tasks_started == tasks_ended
        assert tasks_panicked == 0

    wait_until(10, 0.2, assert_tasks_finish)
