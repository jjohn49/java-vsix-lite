package demo;

import org.junit.jupiter.api.Assertions;
import org.junit.jupiter.api.Test;

public class CalcTest {
    @Test
    void addsSmallNumbers() {
        Assertions.assertEquals(2, new Calc().add(1, 1));
    }

    @Test
    void addsWrongExpectation() {
        Assertions.assertEquals(3, new Calc().add(1, 1));
    }

    @org.junit.Test
    public void legacyAdds() {
        org.junit.Assert.assertEquals(4, new Calc().add(2, 2));
    }
}
