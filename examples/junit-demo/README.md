# junit-demo

A tiny Maven project for trying java-vsix-lite's JUnit support.

Open **this folder** in VS Code (trusted workspace) with the extension
installed, then open the Testing view:

- `CalcTest` (JUnit 5) — 3 passing, 1 deliberately failing
  (`deliberatelyFailingExpectation`), 1 `@Disabled` (shows as skipped)
- `StringUtilsTest` (JUnit 5) — a `@ParameterizedTest` (3 invocations), a
  `@RepeatedTest(3)`, and a `@Nested` class with 2 tests
- `LegacyCalcTest` (JUnit 4) — 2 passing, run through the Vintage engine

Expected result of "Run All": **14 passed, 1 failed, 1 skipped.**

No prior build needed: the first run compiles the sources automatically
(`javac -g`) against the JUnit jars resolved offline from `~/.m2`, and asks
once for consent to download the JUnit Platform Console Launcher if it isn't
cached yet. Set a breakpoint inside any test and use the Debug profile to
stop on it.
