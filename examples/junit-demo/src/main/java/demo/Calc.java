package demo;

public class Calc {
    public int add(int a, int b) {
        int count = 1;
        int x = 10;
        x = "";
        return a + b + count;
    }

    public int divide(int dividend, int divisor) {
        if (divisor == 0) {
            throw new ArithmeticException("division by zero");
        }
        return dividend / divisor;
    }

    private int extracted(int a, int b) {
        int doubled = a * 2;
        int total = doubled + b;
        return total;
    }

    public int demo(int a, int b) {
        int doubled = a * 2;
        int total = doubled + b;
    return total;
    }
}
