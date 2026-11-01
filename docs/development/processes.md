# Processes, jobs and recovery

Capsula runs as two processes, and the brain runs its helpers inside Windows Job Objects so that
nothing it started can outlive it.

```mermaid
flowchart LR
  Shell["capsula.exe (no job)"]
  subgraph BrainJob["brain job (kill on close)"]
    Screen["capsulad --screen"]
    subgraph RunJob["run job, one per run"]
      Child["a run's helper"]
    end
  end
  Brain["capsulad.exe (holds the job handles)"]
  Shell <-->|pipes| Brain
  Brain -.->|owns| BrainJob
```

- **The brain job** has `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and only the brain holds its handle.
  When the brain exits or is killed, Windows ends everything in it.
- **A run job** nests inside the brain job. A helper is created with
  `PROC_THREAD_ATTRIBUTE_JOB_LIST` naming both jobs, so it's inside from its first instruction.
  Ending a run terminates its run job and waits until the job is empty, which ends that run's
  processes and nothing else.
- **The screen worker** sits in the brain job directly, so ending a run never touches it. In
  slice 0 it's a placeholder that only waits.
- Helpers inherit no handles, and nothing secret is ever on their command lines.
- **The shell** isn't in any job, so a brain crash doesn't take the window with it. Each process
  starts the other detached and, where the starting job allows it, outside that job.

## Recovery

| What happens | What the other process does |
|---|---|
| The shell dies | The brain relaunches it after 0.2 s, then 1 s, then 5 s, then stops: a fourth crash within a minute isn't relaunched. A shell that dies before connecting counts |
| The brain dies | Its helpers end with it. The shell restarts it once; a second loss within a minute shows Retry |
| No shell for 30 s, and none being relaunched | The brain exits with its helpers, so a shell that gave up on a slow brain or quit before it was up leaves nothing behind. The next shell starts a new brain |
| A shell is started by hand | It finds the running brain through the pipes and the capability |
| Quit | The shell sends `Quitting`. The brain cancels any pending shell relaunch, ends its jobs, deletes the instance capability and exits, then the shell exits. A shell still connecting starts no brain, and tells one it already started once it connects |

The dock's status panel is transient: closing it isn't Quit. Quit lives in the tray and status
panel and follows the same bounded, idempotent link shutdown. See [presence](presence.md).
