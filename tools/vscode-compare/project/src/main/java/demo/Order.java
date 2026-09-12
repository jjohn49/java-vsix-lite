package demo;

import java.util.Objects;

/** A single order line for a customer. */
public class Order {
    private final String id;
    private final Customer customer;
    private final Money amount;
    private final int quantity;
    private OrderStatus status;

    public Order(String id, Customer customer, Money amount, int quantity) {
        this.id = Objects.requireNonNull(id, "id");
        this.customer = Objects.requireNonNull(customer, "customer");
        this.amount = Objects.requireNonNull(amount, "amount");
        this.quantity = quantity;
        this.status = OrderStatus.DRAFT;
    }

    /** The order identifier. */
    public String id() {
        return id;
    }

    public Customer customer() {
        return customer;
    }

    /** The unit price of this order. */
    public Money amount() {
        return amount;
    }

    public int quantity() {
        return quantity;
    }

    /** Unit price multiplied by quantity. */
    public Money lineTotal() {
        return amount.times(quantity);
    }

    public OrderStatus status() {
        return status;
    }

    public Order withStatus(OrderStatus next) {
        this.status = Objects.requireNonNull(next, "status");
        return this;
    }

    public boolean isBillable() {
        return status.isBillable();
    }

    @Override
    public String toString() {
        return "Order[" + id + ", " + customer.label() + ", " + lineTotal() + ", " + status + "]";
    }
}
