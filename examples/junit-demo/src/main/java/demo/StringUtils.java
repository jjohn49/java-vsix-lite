package demo;

public class StringUtils {
    public String reverse(String input) {
        return new StringBuilder(input).reverse().toString();
    }

    public boolean isPalindrome(String input) {
        String normalized = input.toLowerCase();
        return normalized.equals(reverse(normalized));
    }
}
