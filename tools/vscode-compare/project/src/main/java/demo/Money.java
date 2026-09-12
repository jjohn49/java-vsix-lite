package demo;

import java.util.Objects;

/** A minor-unit money amount in a single currency. */
public final class Money implements Comparable<Money> {
    private final long minorUnits;
    private final String currency;

    private Money(long minorUnits, String currency) {
        this.minorUnits = minorUnits;
        this.currency = currency;
    }

    public static Money of(long minorUnits, String currency) {
        return new Money(minorUnits, currency);
    }

    public static Money zero(String currency) {
        return new Money(0L, currency);
    }

    public long minorUnits() {
        return minorUnits;
    }

    public String currency() {
        return currency;
    }

    /** Sum of two amounts; both sides must share a currency. */
    public Money plus(Money other) {
        if (!currency.equals(other.currency)) {
            throw new IllegalArgumentException("currency mismatch: " + currency + " vs " + other.currency);
        }
        return new Money(minorUnits + other.minorUnits, currency);
    }

    public Money times(int factor) {
        return new Money(minorUnits * factor, currency);
    }

    @Override
    public int compareTo(Money other) {
        return Long.compare(minorUnits, other.minorUnits);
    }

    @Override
    public boolean equals(Object other) {
        if (this == other) {
            return true;
        }
        if (!(other instanceof Money money)) {
            return false;
        }
        return minorUnits == money.minorUnits && currency.equals(money.currency);
    }

    @Override
    public int hashCode() {
        return Objects.hash(minorUnits, currency);
    }

    @Override
    public String toString() {
        return minorUnits + " " + currency;
    }
}
