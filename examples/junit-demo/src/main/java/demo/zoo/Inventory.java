package demo.zoo;

import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;

/** Standard-library heavy: maps, lists, streams over custom types. */
public class Inventory {
    private final Map<String, List<Animal>> byKeeper = new HashMap<>();

    public void assign(String keeper, Animal animal) {
        byKeeper.computeIfAbsent(keeper, k -> new java.util.ArrayList<>()).add(animal);
    }

    public List<String> names(String keeper) {
        return byKeeper.getOrDefault(keeper, List.of()).stream()
                .map(Animal::name)
                .collect(Collectors.toList());
    }

    public int total() {
        return byKeeper.values().stream().mapToInt(List::size).sum();
    }
}
