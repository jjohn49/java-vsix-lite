package demo;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Nested;
import org.junit.jupiter.api.RepeatedTest;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;

class StringUtilsTest {
    private final StringUtils strings = new StringUtils();

    @Test
    void reversesText() {
        Assertions.assertEquals("cba", strings.reverse("abc"));
    }

    @ParameterizedTest
    @ValueSource(strings = { "racecar", "Level", "noon" })
    void recognizesPalindromes(String candidate) {
        Assertions.assertTrue(strings.isPalindrome(candidate), candidate);
    }

    @RepeatedTest(3)
    void reverseIsItsOwnInverse() {
        Assertions.assertEquals("java", strings.reverse(strings.reverse("java")));
    }

    @Nested
    class EdgeCases {
        @Test
        void emptyStringIsAPalindrome() {
            Assertions.assertTrue(strings.isPalindrome(""));
        }

        @Test
        void singleCharacterIsAPalindrome() {
            Assertions.assertTrue(strings.isPalindrome("x"));
        }
    }
}
