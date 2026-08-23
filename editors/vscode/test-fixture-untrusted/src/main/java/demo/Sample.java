package demo;

// A well-formed file with no build file (no pom.xml/build.gradle) anywhere in
// this fixture -- the untrusted-workspace suite only needs to prove the
// pure-Rust default tier (semantic tokens, outline) still serves a valid
// file; it must never need a JDK/Maven/Gradle to be present.
public class Sample {
    private final String name;

    public Sample(String name) {
        this.name = name;
    }

    public String greet() {
        return "Hello, " + name;
    }
}
