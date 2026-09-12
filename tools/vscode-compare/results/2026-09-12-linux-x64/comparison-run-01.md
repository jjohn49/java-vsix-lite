# java-vsix-lite vs redhat.java — baseline comparison

- VS Code: `1.137.0`
- java-vsix-lite: `0.1.9`
- redhat.java: `1.57.2026090408`
- baseline: no extension installed (container + VS Code + probe suite only)
- settle before sampling: 30s
- probes: 13 (ours vs redhat — identical 5, different 8)

## Measured window per flavor

| flavor | probe-run duration |
| --- | --- |
| baseline (no extension) | 35767 ms |
| java-vsix-lite | 13122 ms |
| redhat.java | 21672 ms |

## Probes

| probe | ours vs redhat | baseline (ms) | java-vsix-lite (ms) | redhat.java (ms) | java-vsix-lite | redhat.java |
| --- | --- | --- | --- | --- | --- | --- |
| completion.orders | differs | 10 | 11 | 231 | `["add","count","currency","currency","items","snapshot","total","totalMoney"]` | `["add","count","currency","equals","getClass","hashCode","notify","notifyAll","snapshot","toString","total","totalMoney","wait","wait","wait"]` |
| definition.total | same | 2 | 5 | 12 | `["${PROJECT}/src/main/java/demo/Orders.java#L22:16"]` | `["${PROJECT}/src/main/java/demo/Orders.java#L22:16"]` |
| diagnostics.clean | same | 0 | 0 | 0 | `[]` | `[]` |
| edit.crossFileRename | differs | 5074 | 153 | 1562 | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"java-vsix-lite","code":null,"message":"Cannot resolve method 'total'","line":10,"character":34}]}` | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"Java","code":"67108964","message":"The method total() is undefined for the type Orders","line":10,"character":34}]}` |
| edit.dependencyMisuse | differs | 5079 | 101 | 1569 | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"java-vsix-lite","code":null,"message":"Cannot resolve method 'copyOfRange'","line":44,"character":29}]}` | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"Java","code":"67108964","message":"The method copyOfRange(List<Order>) is undefined for the type ImmutableList","line":44,"character":29}]}` |
| edit.localTypeError | differs | 5075 | 36 | 1463 | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"java-vsix-lite","code":"jvl.incompatibleAssignment","message":"incompatible types: int cannot be converted to String","line":22,"character":21},{"severity":"Error","source":"java-vsix-lite","code":"jvl.incompatibleReturn","message":"i...` | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"Java","code":"16777233","message":"Type mismatch: cannot convert from int to String","line":22,"character":21},{"severity":"Error","source":"Java","code":"16777235","message":"Type mismatch: cannot convert from String to int","line":2...` |
| edit.removedImport | differs | 5099 | 6127 | 1077 | `{"outcome":"answered-without-match","diagnostics":[]}` | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"Java","code":"16777218","message":"ImmutableList cannot be resolved to a type","line":43,"character":11},{"severity":"Error","source":"Java","code":"570425394","message":"ImmutableList cannot be resolved","line":44,"character":15}]}` |
| edit.revertAll | same | 44 | 35 | 36 | `{"outcome":"clean","diagnostics":[]}` | `{"outcome":"clean","diagnostics":[]}` |
| edit.unknownMember | differs | 5077 | 65 | 1875 | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"java-vsix-lite","code":null,"message":"Cannot resolve method 'price'","line":25,"character":29}]}` | `{"outcome":"matched","diagnostics":[{"severity":"Error","source":"Java","code":"67108964","message":"The method price() is undefined for the type Order","line":25,"character":29}]}` |
| edit.validAddition | same | 5085 | 6182 | 7126 | `{"outcome":"clean","diagnostics":[]}` | `{"outcome":"clean","diagnostics":[]}` |
| hover.total | differs | 2 | 14 | 457 | `["```java\npublic int total()\n```\n\nSum of every billable order's line total."]` | `["```java\nint demo.Orders.total()\n```","Sum of every billable order's line total.","Source: *[orders](file:///work/tools/vscode-compare/project/src/main/java/demo/Orders.java#22)*"]` |
| server.ready | same | 5031 | 36 | 6005 | `true` | `true` |
| symbols.orders | differs | 2 | 22 | 4 | `["Class Orders","Constructor Orders","Field currency","Field items","Method add","Method count","Method currency","Method snapshot","Method total","Method totalMoney"]` | `["Class Orders","Constructor Orders(String)","Field currency","Field items","Method add(Order)","Method count()","Method currency()","Method snapshot()","Method total()","Method totalMoney()","Package demo"]` |
