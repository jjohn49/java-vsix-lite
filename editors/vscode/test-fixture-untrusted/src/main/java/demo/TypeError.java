package demo;

// Same shape as ../../../../test-fixture/src/main/java/demo/Broken.java: a
// type error only `javac` can catch (the pure-Rust default tier has no type
// checker of its own). If this file ever ends up with a `source === "javac"`
// diagnostic while the workspace is untrusted, the trust gate in
// `extension.ts` (`javacBackgroundCheckEnabled` / `checkProject`) has broken.
public class TypeError {
    void m() {
        int x = "hello";
    }
}
