package dev.openprofiler.api;

import java.lang.annotation.Documented;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Turns a method into a custom probe: when the OpenProfiler agent is loaded, every call is
 * recorded as a call of probe {@code custom.<value>} with the payload built from {@link #payload()}
 * (hot spots, events, call trees per payload). Without an agent the annotation has no effect.
 *
 * <pre>{@code
 * @Probe(value = "orders", payload = "place order {0}")
 * void place(String orderId) { ... }
 * }</pre>
 */
@Documented
// RUNTIME so the annotation survives in classes the agent retransforms after a dynamic attach.
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.METHOD, ElementType.CONSTRUCTOR})
public @interface Probe {
    /** Probe name; calls of all methods with the same name are shown together. */
    String value() default "custom";

    /**
     * Payload template: {@code {0}}, {@code {1}}, ... insert arguments, {@code {this}} the receiver.
     * Empty: the class and method name.
     */
    String payload() default "";
}
