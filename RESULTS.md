# Scorer submissions

| # | Time (ET) | Commit | Receipt | Result |
|---|-----------|--------|---------|--------|
| 1 | Fri 7:30 PM | 2eefc7b (engine v1, scalar) | BB-6BD38C38 | CORRECT, 2.81 s (entered in final) |
| 2 | Fri 8:09 PM | c3b46cc (tuning round 1) | BB-BA04056F | CORRECT, 2.03 s (best, entered in final) |

Local/CI reference: 20M events, 4 vCPU AMD EPYC runner: engine 4.7 s, via docker 6.5 s; Apple M5: 4.9 s.
Leaderboard at submit time: leaders 1.3-2 s on practice data; baseline 171.9 s.
CI (4 vCPU AMD EPYC, 20M): v1 4.65 s -> round 1 2.8 s; peak RSS 1.26 GB; output matches oracle.
