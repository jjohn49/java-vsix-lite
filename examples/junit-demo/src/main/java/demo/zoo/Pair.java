package demo.zoo;

/** A generic record with two type parameters. */
public record Pair<A, B>(A first, B second) {
    public static <X, Y> Pair<X, Y> of(X first, Y second) {
        return new Pair<>(first, second);
    }

    public Pair<B, A> swap() {
        return new Pair<>(second, first);
    }
}
