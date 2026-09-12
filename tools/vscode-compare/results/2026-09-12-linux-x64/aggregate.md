# java-vsix-lite vs redhat.java — aggregate of 9 runs

- host: `Linux 7.1.7-200.fc44.x86_64 x86_64`
- docker: `29.7.2`, 8 CPUs available to containers
- image: `sha256:45508fff1517` (identical across all runs)
- settle before sampling: 30s
- runs: 9

Cells show **median [min–max]** across runs. A single number means n=1.

## Cost per flavor

`core-s` is CPU time actually consumed over the measured window (the
integral of the sampler's CPU% trace), which is comparable across flavors
whose windows differ in length — unlike peak or mean CPU%.

| flavor | window (ms) | CPU (core-s) | peak mem (MB) | CPU-capped | retries |
| --- | --- | --- | --- | --- | --- |
| baseline | 35826 [35708–35945] | 12.2 [12–12.3] | 1008.7 [994.6–1014.7] | 0% | 0 |
| java-vsix-lite | 13126 [13049–13684] | 10.5 [10.3–10.9] | 1042.7 [1025.9–1072.1] | 0% | 0 |
| redhat.java | 21584 [21416–21905] | 68 [65.5–69.8] | 1868.3 [1825.4–2013] | 4.2% | 0 |

## Probe latency

`n` counts runs that produced a timing; a probe that timed out contributes
no latency, so its `n` is lower and its timeout count is shown instead.

| probe | java-vsix-lite (ms) | redhat.java (ms) | baseline (ms) | stable? |
| --- | --- | --- | --- | --- |
| completion.orders | 11 [6–19] | 236 [208–275] | 10 [3–11] | yes |
| definition.total | 9 [3–19] | 17 [11–28] | 2 [1–2] | yes |
| diagnostics.clean | 0 [0–1] | 0 [0–0] | 0 [0–1] | yes |
| edit.crossFileRename | 149 [72–155] | 2068 [1562–2190] | — (9× timed out) | yes |
| edit.dependencyMisuse | 99 [61–208] | 1049 [1039–1569] | — (9× timed out) | yes |
| edit.localTypeError | 48 [36–68] | 1461 [1367–1469] | — (9× timed out) | yes |
| edit.removedImport | 6173 [6083–6271] | 1044 [1041–1077] | — (9× timed out) | yes |
| edit.revertAll | 39 [33–41] | 50 [36–58] | 41 [35–44] | yes |
| edit.unknownMember | 86 [56–151] | 1902 [1770–2013] | — (9× timed out) | yes |
| edit.validAddition | 6092 [6073–6697] | 7126 [7092–7259] | 5085 [5079–5101] (no response) | yes |
| hover.total | 10 [5–16] | 457 [379–505] | 2 [2–7] | yes |
| server.ready | 36 [23–77] | 5950 [5670–6071] | — (9× timed out) | yes |
| symbols.orders | 10 [2–22] | 6 [1–13] | 1 [1–2] | yes |

## Headline

Over 9 runs, java-vsix-lite consumed a median of 10.53 core-seconds against redhat.java's 67.98 (6.5×), peaking at 1042.7 MB against 1868.3 MB (1.8×).
