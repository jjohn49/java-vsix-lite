package demo.zoo;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Optional;

/**
 * Manual test bed. Everything here compiles. Each "UNCOMMENT" line is one
 * mistake to try: uncomment it and watch for a squiggle without saving.
 * Lines marked "javac only" are expected to need the save-triggered compiler.
 */
public class Playground {
    void customClasses() {
        Dog rex = new Dog("Rex");
        Animal a = rex;
        Cat tom = new Cat("Tom");
        // UNCOMMENT: wrong custom type
        // Dog notADog = tom;
        // UNCOMMENT: sibling types are unrelated
        // Cat notACat = rex;
        // UNCOMMENT: no such constructor
        // Dog nameless = new Dog();
        // UNCOMMENT: abstract class
        // Animal abstractOne = new Animal("x");
        // UNCOMMENT: interface
        // Named n = new Named();
        // UNCOMMENT: record constructor arity
        // Toy t = new Toy("ball");
    }

    void inheritedAndOverridden() {
        Dog rex = new Dog("Rex");
        String s1 = rex.name();        // inherited from Animal
        String s2 = rex.describe();    // inherited, not overridden in Dog
        String s3 = rex.sound();       // overridden in Dog
        Dog same = rex.fetch(new Toy("rope", 2));
        Cat tom = new Cat("Tom");
        String s4 = tom.describe();    // overridden in Cat
        // UNCOMMENT: inherited method's return type is String, not int
        // int bad = rex.name();
        // UNCOMMENT: fetch() takes a Toy, not a String
        // rex.fetch("rope");
        // UNCOMMENT: Cat has no fetch()
        // tom.fetch(new Toy("rope", 2));
        // UNCOMMENT: fetch() returns Dog, not Cat
        // Cat c = rex.fetch(new Toy("rope", 2));
    }

    void genericsCustom() {
        Shelter<Dog> dogs = new Shelter<>();
        dogs.admit(new Dog("Rex"));
        Dog first = dogs.first();
        Optional<Dog> found = dogs.find("Rex");
        List<Dog> all = dogs.all();
        Kennel kennel = new Kennel();
        Shelter<Dog> asShelter = kennel;      // subclass of Shelter<Dog>
        Dog kenneled = kennel.first();
        Shelter<Cat> cats = Shelter.of(new Cat("Tom"));   // A inferred as Cat
        Pair<String, Integer> p = Pair.of("age", 3);
        Pair<Integer, String> swapped = p.swap();
        // UNCOMMENT: T is Dog, a Cat cannot be admitted
        // dogs.admit(new Cat("Tom"));
        // UNCOMMENT: first() returns Dog
        // Cat notACat = dogs.first();
        // UNCOMMENT: generics are invariant: Shelter<Dog> is not a Shelter<Animal>
        // Shelter<Animal> animals = dogs;
        // UNCOMMENT: explicit type argument mismatch
        // Shelter<Dog> wrong = new Shelter<Cat>();
        // UNCOMMENT: bound violated: Toy is not an Animal
        // Shelter<Toy> toys = new Shelter<>();
        // UNCOMMENT: Kennel is a Shelter<Dog>, not a Shelter<Cat>
        // Shelter<Cat> notCats = kennel;
        // UNCOMMENT: swap() flips the type arguments
        // Pair<String, Integer> notSwapped = p.swap();
    }

    void genericsWildcards() {
        Shelter<Dog> dogs = new Shelter<>();
        Shelter<? extends Animal> anyAnimals = dogs;     // wildcard accepts a subtype
        Animal peek = anyAnimals.first();                // reads as the bound
        List<? super Dog> sink = new ArrayList<Animal>();
        sink.add(new Dog("Rex"));
        // UNCOMMENT: cannot admit into a "? extends" (javac only today)
        // anyAnimals.admit(new Dog("Rex"));
        // UNCOMMENT: Toy is outside the bound
        // Shelter<? extends Animal> notAnimals = new Shelter<Toy>();
    }

    void standardLibrary() {
        List<String> names = new ArrayList<>();
        names.add("Rex");
        String first = names.get(0);
        int len = first.length();
        Map<String, Integer> ages = Map.of("Rex", 3);
        Integer age = ages.get("Rex");
        StringBuilder sb = new StringBuilder().append("a").append(1);
        String built = sb.toString();
        // UNCOMMENT: List<String>.get returns String
        // Integer notAnInt = names.get(0);
        // UNCOMMENT: wrong argument type
        // names.add(42);
        // UNCOMMENT: invariance on stdlib generics
        // List<Object> objects = names;
        // UNCOMMENT: String has no such method
        // first.lenght();
        // UNCOMMENT: length() returns int
        // String notAString = first.length();
        // UNCOMMENT: Map.get returns Integer here
        // String notAge = ages.get("Rex");
    }

    void mixed() {
        Inventory inv = new Inventory();
        inv.assign("alice", new Dog("Rex"));
        inv.assign("alice", new Cat("Tom"));
        List<String> names = inv.names("alice");
        int total = inv.total();
        Shelter<Dog> dogs = Shelter.of(new Dog("Rex"), new Dog("Fido"));
        Map<String, Shelter<Dog>> byRoom = Map.of("A", dogs);
        Dog fromMap = byRoom.get("A").first();
        Optional<Dog> maybe = dogs.find("Rex");
        String name = maybe.map(Animal::name).orElse("none");
        // UNCOMMENT: Inventory takes an Animal, not a Toy
        // inv.assign("alice", new Toy("ball", 1));
        // UNCOMMENT: names() returns List<String>
        // List<Integer> wrong = inv.names("alice");
        // UNCOMMENT: chained generic result is Dog
        // Cat notACat = byRoom.get("A").first();
        // UNCOMMENT: mixing Dog and Cat into a Shelter.of() has no single A (javac only today)
        // Shelter<Dog> mixedUp = Shelter.of(new Dog("Rex"), new Cat("Tom"));
    }
}
