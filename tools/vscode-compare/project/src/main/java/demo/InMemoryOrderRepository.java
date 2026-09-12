package demo;

import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;

import com.google.common.collect.ImmutableList;

/** A map-backed {@link OrderRepository}, preserving insertion order. */
public class InMemoryOrderRepository implements OrderRepository {
    private final Map<String, Order> byId = new LinkedHashMap<>();

    @Override
    public void save(Order order) {
        byId.put(order.id(), order);
    }

    @Override
    public Optional<Order> findById(String id) {
        return Optional.ofNullable(byId.get(id));
    }

    @Override
    public List<Order> findByStatus(OrderStatus status) {
        ImmutableList.Builder<Order> matches = ImmutableList.builder();
        for (Order order : byId.values()) {
            if (order.status() == status) {
                matches.add(order);
            }
        }
        return matches.build();
    }

    @Override
    public int size() {
        return byId.size();
    }
}
