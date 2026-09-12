package demo;

import java.util.List;
import java.util.Optional;

import com.google.common.collect.ImmutableMap;
import org.apache.commons.lang3.StringUtils;

/** Application-level operations over a {@link OrderRepository}. */
public class OrderService {
    private final OrderRepository repository;
    private final String currency;

    public OrderService(OrderRepository repository, String currency) {
        this.repository = repository;
        this.currency = currency;
    }

    /** Place a new order for a customer and persist it. */
    public Order place(String id, Customer customer, long unitMinorUnits, int quantity) {
        Order order = new Order(id, customer, Money.of(unitMinorUnits, currency), quantity)
                .withStatus(OrderStatus.PLACED);
        repository.save(order);
        return order;
    }

    public Optional<Order> find(String id) {
        return StringUtils.isBlank(id) ? Optional.empty() : repository.findById(id);
    }

    /** Total of every billable order currently stored. */
    public Money billableTotal() {
        Money running = Money.zero(currency);
        for (OrderStatus status : OrderStatus.values()) {
            if (!status.isBillable()) {
                continue;
            }
            for (Order order : repository.findByStatus(status)) {
                running = running.plus(order.lineTotal());
            }
        }
        return running;
    }

    /** How many orders sit in each status, including empty statuses. */
    public ImmutableMap<OrderStatus, Integer> countsByStatus() {
        ImmutableMap.Builder<OrderStatus, Integer> counts = ImmutableMap.builder();
        for (OrderStatus status : OrderStatus.values()) {
            List<Order> matches = repository.findByStatus(status);
            counts.put(status, matches.size());
        }
        return counts.build();
    }

    public String describe() {
        return StringUtils.joinWith(
                " / ",
                "orders=" + repository.size(),
                "billable=" + billableTotal(),
                "currency=" + currency);
    }
}
