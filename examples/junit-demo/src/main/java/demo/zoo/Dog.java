package demo.zoo;

/** Overrides sound(), inherits describe(), adds its own method. */
public class Dog extends Animal {
    public Dog(String name) {
        super(name);
    }

    @Override
    public String sound() {
        return "woof";
    }

    public Dog fetch(Toy toy) {
        return this;
    }
}
