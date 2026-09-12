package demo.zoo;

/** A record: canonical constructor, accessors, generics-free. */
public record Toy(String label, int size) {
    public Toy {
        if (size < 0) {
            throw new IllegalArgumentException("size");
        }
    }
}
