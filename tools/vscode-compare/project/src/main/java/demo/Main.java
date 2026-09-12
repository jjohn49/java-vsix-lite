package demo;

public class Main {
    public static void main(String[] args) {
        Customer alice = new Customer("c-1", "Alice", "alice@example.com");
        Customer bob = new Customer("c-2", "", "bob@example.com");

        Orders orders = new Orders("USD");
        orders.add(new Order("a", alice, Money.of(250, "USD"), 2).withStatus(OrderStatus.PLACED));
        orders.add(new Order("b", bob, Money.of(1000, "USD"), 1).withStatus(OrderStatus.SHIPPED));
        System.out.println(orders.total());
        System.out.println(orders.totalMoney());
        System.out.println(orders.snapshot().size());

        OrderService service = new OrderService(new InMemoryOrderRepository(), "USD");
        service.place("s-1", alice, 499, 3);
        service.place("s-2", bob, 150, 10);
        System.out.println(service.describe());
        System.out.println(service.countsByStatus());
        System.out.println(service.find("s-1").map(Order::toString).orElse("missing"));
        System.out.println(bob.label());
    }
}
