package demo;

import java.util.ArrayList;
import java.util.List;

public class Broken {
    void m() {
        List<Integer> nums = new ArrayList<Integer>();
        List<String> names = nums;
    }
}
