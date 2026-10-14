# starla-scheduler

Measurement scheduling and execution for Starla.

## Features

- **Slot scheduling**: a recurring measurement runs on the reference probe's slots — cycle `k` at `start_time + k * interval`, with a uniform `spread` offset drawn each cycle
- **Crontab lines**: the controller's `CRONTAB`/`CRONLINE` set, kept per file as `eperd` keeps it
- **One-offs**: `ExecuteNow` runs a measurement immediately
- **Concurrent execution**: measurements run in parallel
- **Result forwarding**: finished results go to the `ResultHandler`

## Usage

```rust
use starla_scheduler::{Scheduler, SchedulerCommand, MeasurementJob, MeasurementSpec};

let mut scheduler = Scheduler::new(probe_id, metrics);
scheduler.set_result_handler(result_handler.clone());

// A sender the controller (and the telnet interface) feeds commands to.
let tx = scheduler.command_sender();

tx.send(SchedulerCommand::Schedule(MeasurementJob {
    msm_id: 12345,
    interval: 300,          // every 5 minutes
    start_time: now,
    end_time: now + 86400,  // 24 hours
    spread: 60,
    spec: MeasurementSpec::Ping(ping_spec),
})).await?;

// Runs until its cancel token is cancelled (scheduler.cancel_token()).
scheduler.run().await;
```

## Commands

| Command | Description |
|---------|-------------|
| `Schedule(job)` | A recurring measurement outside any crontab |
| `Crontab(crontab)` | The file's recurring lines, replaced as a whole |
| `Stop(msm_id)` | Stop every line of a measurement |
| `ExecuteNow(job)` | Run a one-shot measurement immediately |

## Measurement Types

All RIPE Atlas measurement types are supported:
- `PingJobSpec`
- `TracerouteJobSpec`
- `DnsJobSpec`
- `HttpJobSpec`
- `TlsJobSpec`
- `NtpJobSpec`
