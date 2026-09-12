package demo.zoo;

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;

/** A bounded generic container over the Animal hierarchy. */
public class Shelter<T extends Animal> {
    private final List<T> residents = new ArrayList<>();

    public void admit(T animal) {
        residents.add(animal);
    }

    public T first() {
        return residents.get(0);
    }

    public Optional<T> find(String name) {
        return residents.stream().filter(a -> a.name().equals(name)).findFirst();
    }

    public List<T> all() {
        return residents;
    }

    public int count() {
        return residents.size();
    }

    /** Generic static factory: T is inferred from the arguments. */
    @SafeVarargs
    public static <A extends Animal> Shelter<A> of(A... animals) {
        Shelter<A> shelter = new Shelter<>();
        for (A a : animals) {
            shelter.admit(a);
        }
        return shelter;
    }
}
