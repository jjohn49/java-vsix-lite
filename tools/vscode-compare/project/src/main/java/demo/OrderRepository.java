package demo;

import java.util.List;
import java.util.Optional;

/** Storage abstraction over orders. */
public interface OrderRepository {

    void save(Order order);

    Optional<Order> findById(String id);

    List<Order> findByStatus(OrderStatus status);

    int size();
}
