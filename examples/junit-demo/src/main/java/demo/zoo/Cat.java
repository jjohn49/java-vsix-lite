package demo.zoo;

/** Overrides both sound() and the inherited describe(). */
public class Cat extends Animal {
    public Cat(String name) {
        super(name);
    }

    @Override
    public String sound() {
        return "meow";
    }

    @Override
    public String describe() {
        return name() + " ignores you";
    }
}
