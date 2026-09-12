package demo.zoo;

/** Base class: one abstract method, one concrete one to inherit or override. */
public abstract class Animal implements Named {
    private final String name;

    protected Animal(String name) {
        this.name = name;
    }

    @Override
    public String name() {
        return name;
    }

    public abstract String sound();

    /** Inherited as-is by Dog, overridden by Cat. */
    public String describe() {
        return name + " says " + sound();
    }
}
