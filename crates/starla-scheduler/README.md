# starla-scheduler

Measurement scheduling and execution for Starla.

## Features

- **Job Scheduling**: Execute measurements at specified times
- **Recurring Measurements**: Support for interval-based scheduling
- **Spread/Jitter**: Randomized start times to avoid thundering herd
- **Concurrent Execution**: Run multiple measurements in parallel
- **Result Forwarding**: Automatic forwarding to result handler

## Usage

```rust
use starla_scheduler::{Scheduler, SchedulerCommand, MeasurementJob, MeasurementSpec};

let mut scheduler = Scheduler::new(db, probe_id);
scheduler.set_result_handler(result_handler);

// Get command channel
let tx = scheduler.command_sender();

// Schedule a measurement
tx.send(SchedulerCommand::Schedule(MeasurementJob {
    msm_id: 12345,
    interval: 300,  // Every 5 minutes
    start_time: now,
    end_time: now + 86400,  // 24 hours
    spread: 60,
    spec: MeasurementSpec::Ping(ping_spec),
})).await?;

// Run the scheduler
scheduler.run().await;
```

## Commands

| Command | Description |
|---------|-------------|
| `Schedule(job)` | Add a recurring measurement |
| `ExecuteNow(job)` | Run a one-shot measurement immediately |
| `Stop(msm_id)` | Stop a scheduled measurement |

## Measurement Types

All RIPE Atlas measurement types are supported:
- `PingJobSpec`
- `TracerouteJobSpec`
- `DnsJobSpec`
- `HttpJobSpec`
- `TlsJobSpec`
- `NtpJobSpec`
