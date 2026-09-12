package demo;

import java.util.ArrayList;
import java.util.List;

import com.google.common.collect.ImmutableList;

/** An in-memory collection of orders. */
public class Orders {
    private final List<Order> items = new ArrayList<>();
    private final String currency;

    public Orders(String currency) {
        this.currency = currency;
    }

    public void add(Order order) {
        items.add(order);
    }

    /** Sum of every billable order's line total. */
    public int total() {
        int sum = 0;
        for (Order order : items) {
            if (order.isBillable()) {
                sum += order.lineTotal().minorUnits();
            }
        }
        return sum;
    }

    /** The same sum, as a {@link Money} in this collection's currency. */
    public Money totalMoney() {
        Money running = Money.zero(currency);
        for (Order order : items) {
            if (order.isBillable()) {
                running = running.plus(order.lineTotal());
            }
        }
        return running;
    }

    /** A defensive snapshot of the orders, in insertion order. */
    public ImmutableList<Order> snapshot() {
        return ImmutableList.copyOf(items);
    }

    public int count() {
        return items.size();
    }

    public String currency() {
        return currency;
    }
}
