package demo.zoo;

/** A subclass of a generic class with the type argument fixed. */
public class Kennel extends Shelter<Dog> {
    /** Override of an inherited generic method, specialized to Dog. */
    @Override
    public Dog first() {
        Dog d = super.first();
        return d.fetch(new Toy("ball", 1));
    }
}
