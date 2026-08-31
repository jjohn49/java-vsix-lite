package demo;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Disabled;
import org.junit.jupiter.api.Test;

class CalcTest {
    private final Calc calc = new Calc();

    @Test
    void addsPositiveNumbers() {
        Assertions.assertEquals(5, calc.add(2, 3));
    }

    @Test
    void addsNegativeNumbers() {
        Assertions.assertEquals(-4, calc.add(-1, -3));
    }

    @Test
    void deliberatelyFailingExpectation() {
        // Intentionally wrong — shows a red test with its failure message.
        Assertions.assertEquals(3, calc.add(1, 1), "math still works the old way");
    }

    @Test
    void divideByZeroThrows() {
        Assertions.assertThrows(ArithmeticException.class, () -> calc.divide(1, 0));
    }

    @Disabled("shows up as skipped in the Test Explorer")
    @Test
    void notReadyYet() {
        Assertions.assertEquals(0, calc.add(0, 0));
    }
}
