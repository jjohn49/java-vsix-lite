package demo;

// Unlike TypeError.java (a javac-only error), this incompatibility is on a
// method RETURN, which the pure-Rust default tier's return check proves
// natively (`jvl.incompatibleReturn`). It must be flagged even in an
// untrusted workspace with no JDK and no automatic compiler — the native
// tier is not trust-gated.
public class ReturnTypeError {
    int code() {
        return "bad";
    }
}
