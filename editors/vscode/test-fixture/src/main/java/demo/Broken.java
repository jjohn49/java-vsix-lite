package demo;

import java.util.ArrayList;
import java.util.List;

public class Broken {
    void m() {
        List<Integer> nums = new ArrayList<Integer>();
        nums.add(1);
    }

    // Only javac reports this: the native tier does no exception-flow analysis.
    void unhandled() {
        throw new java.io.IOException("unreported checked exception");
    }
}
