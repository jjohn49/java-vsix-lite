package demo;

import org.apache.commons.lang3.StringUtils;

/** A customer an order belongs to. */
public record Customer(String id, String displayName, String email) {

    public Customer {
        if (StringUtils.isBlank(id)) {
            throw new IllegalArgumentException("customer id is required");
        }
    }

    /** Display name, falling back to the email local part, then the id. */
    public String label() {
        if (StringUtils.isNotBlank(displayName)) {
            return displayName;
        }
        String local = StringUtils.substringBefore(email, "@");
        return StringUtils.defaultIfBlank(local, id);
    }

    public boolean hasEmail() {
        return StringUtils.isNotBlank(email);
    }
}
