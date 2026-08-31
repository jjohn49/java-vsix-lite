package demo;

public class Calc {
    public int add(int a, int b) {
        return a + b;
    }

    public int divide(int dividend, int divisor) {
        if (divisor == 0) {
            throw new ArithmeticException("division by zero");
        }
        return dividend / divisor;
    }
}
