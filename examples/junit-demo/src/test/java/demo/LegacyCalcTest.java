package demo;

import org.junit.Assert;
import org.junit.Test;

/** JUnit 4 — runs through the bundled Vintage engine, no config needed. */
public class LegacyCalcTest {
    @Test
    public void addsWithTheOldApi() {
        Assert.assertEquals(4, new Calc().add(2, 2));
    }

    @Test
    public void divideRoundsTowardZero() {
        Assert.assertEquals(2, new Calc().divide(5, 2));
    }
}
