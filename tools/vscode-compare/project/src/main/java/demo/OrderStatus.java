package demo;

/** Lifecycle state of an order. */
public enum OrderStatus {
    DRAFT(false),
    PLACED(true),
    SHIPPED(true),
    CANCELLED(false);

    private final boolean billable;

    OrderStatus(boolean billable) {
        this.billable = billable;
    }

    /** Whether an order in this state counts toward revenue. */
    public boolean isBillable() {
        return billable;
    }
}
