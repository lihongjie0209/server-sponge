/// Detailed breakdown of a PID computation for logging.
#[derive(Debug, Clone, Copy)]
pub struct PidOutput {
    pub error: f64,
    pub kp_used: f64,
    pub p_term: f64,
    pub i_term: f64,
    pub d_term: f64,
    pub integral_acc: f64,
    pub raw_output: f64,
    pub clamped_output: f64,
    pub anti_windup_active: bool,
}

impl std::fmt::Display for PidOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "error={:+.2}% Kp={:.1} P={:+.2} I={:+.2} D={:+.2} raw={:+.2} out={:+.2}{}",
            self.error,
            self.kp_used,
            self.p_term,
            self.i_term,
            self.d_term,
            self.raw_output,
            self.clamped_output,
            if self.anti_windup_active { " [anti-windup]" } else { "" }
        )
    }
}

/// PID Controller with asymmetric gains and anti-windup protection.
///
/// Positive error means we need to allocate more (under target).
/// Negative error means we need to release (over target or pressure).
pub struct PidController {
    /// Proportional gain for positive error (allocation)
    kp_alloc: f64,
    /// Proportional gain for negative error (release) — typically larger
    kp_release: f64,
    /// Integral gain
    ki: f64,
    /// Derivative gain
    kd: f64,
    /// Output limit (max absolute chunks per cycle)
    limit: f64,
    /// Accumulated integral term
    integral: f64,
    /// Previous error for derivative calculation
    prev_error: f64,
    /// Whether the controller has seen at least one update
    initialized: bool,
}

impl PidController {
    /// Create a new PID controller.
    /// `kp` is the base proportional gain; release direction uses `kp * release_multiplier`.
    pub fn new(kp: f64, ki: f64, kd: f64, limit: f64) -> Self {
        Self {
            kp_alloc: kp,
            kp_release: kp * 2.0, // Release responds 2x faster than allocate
            ki,
            kd,
            limit,
            integral: 0.0,
            prev_error: 0.0,
            initialized: false,
        }
    }

    /// Compute the control output given the current error.
    /// Returns a detailed PidOutput with full breakdown for logging.
    pub fn update(&mut self, error: f64, pool_is_empty: bool) -> PidOutput {
        // Select asymmetric Kp
        let kp = if error >= 0.0 {
            self.kp_alloc
        } else {
            self.kp_release
        };

        // Anti-windup: don't accumulate integral when:
        // 1. Pool is empty AND error is negative (can't release more)
        let anti_windup_active = pool_is_empty && error < 0.0;

        if !anti_windup_active {
            self.integral += error;
            // Clamp integral to prevent runaway
            let integral_limit = self.limit / self.ki.max(0.001);
            self.integral = self.integral.clamp(-integral_limit, integral_limit);
        }

        // Derivative term
        let derivative = if self.initialized {
            error - self.prev_error
        } else {
            0.0
        };

        self.prev_error = error;
        self.initialized = true;

        // PID terms
        let p_term = kp * error;
        let i_term = self.ki * self.integral;
        let d_term = self.kd * derivative;
        let raw_output = p_term + i_term + d_term;
        let clamped_output = raw_output.clamp(-self.limit, self.limit);

        PidOutput {
            error,
            kp_used: kp,
            p_term,
            i_term,
            d_term,
            integral_acc: self.integral,
            raw_output,
            clamped_output,
            anti_windup_active,
        }
    }

    /// Reset the integral term (useful after mode transitions)
    pub fn reset_integral(&mut self) {
        self.integral = 0.0;
    }

    /// Reset the entire controller state
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.prev_error = 0.0;
        self.initialized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── P-term tests ──

    #[test]
    fn test_positive_error_allocates() {
        let mut pid = PidController::new(2.0, 0.1, 0.5, 100.0);
        let out = pid.update(10.0, false);
        assert!(out.clamped_output > 0.0);
        assert!(out.p_term > 0.0);
    }

    #[test]
    fn test_negative_error_releases() {
        let mut pid = PidController::new(2.0, 0.1, 0.5, 100.0);
        let out = pid.update(-10.0, false);
        assert!(out.clamped_output < 0.0);
        assert!(out.p_term < 0.0);
    }

    #[test]
    fn test_zero_error_zero_output() {
        let mut pid = PidController::new(2.0, 0.0, 0.0, 100.0);
        let out = pid.update(0.0, false);
        assert_eq!(out.clamped_output, 0.0);
        assert_eq!(out.p_term, 0.0);
    }

    #[test]
    fn test_p_term_proportional_to_error() {
        let mut pid = PidController::new(3.0, 0.0, 0.0, 1000.0);
        let out = pid.update(10.0, false);
        // Kp_alloc = 3.0, error = 10 => P = 30
        assert!((out.p_term - 30.0).abs() < 0.01);
    }

    // ── Asymmetric gain tests ──

    #[test]
    fn test_asymmetric_gains() {
        let mut pid1 = PidController::new(2.0, 0.0, 0.0, 1000.0);
        let mut pid2 = PidController::new(2.0, 0.0, 0.0, 1000.0);
        let alloc = pid1.update(10.0, false);
        let release = pid2.update(-10.0, false);
        assert!(release.clamped_output.abs() > alloc.clamped_output.abs());
        // alloc Kp=2.0, release Kp=4.0
        assert!((alloc.kp_used - 2.0).abs() < 0.01);
        assert!((release.kp_used - 4.0).abs() < 0.01);
    }

    #[test]
    fn test_release_gain_is_double_alloc() {
        let mut pid = PidController::new(5.0, 0.0, 0.0, 1000.0);
        let alloc = pid.update(1.0, false);
        let mut pid = PidController::new(5.0, 0.0, 0.0, 1000.0);
        let release = pid.update(-1.0, false);
        // |release| / |alloc| should be 2.0
        let ratio = release.clamped_output.abs() / alloc.clamped_output.abs();
        assert!((ratio - 2.0).abs() < 0.01, "ratio was {}", ratio);
    }

    // ── Clamping tests ──

    #[test]
    fn test_output_clamped_positive() {
        let mut pid = PidController::new(200.0, 0.0, 0.0, 10.0);
        let out = pid.update(100.0, false);
        assert!((out.clamped_output - 10.0).abs() < 0.01);
        assert!(out.raw_output > 10.0);
    }

    #[test]
    fn test_output_clamped_negative() {
        let mut pid = PidController::new(200.0, 0.0, 0.0, 10.0);
        let out = pid.update(-100.0, false);
        assert!((out.clamped_output - (-10.0)).abs() < 0.01);
        assert!(out.raw_output < -10.0);
    }

    // ── I-term / integral tests ──

    #[test]
    fn test_integral_accumulates() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, 1000.0);
        pid.update(5.0, false);
        pid.update(5.0, false);
        let out = pid.update(5.0, false);
        // integral should be 15.0, i_term = 1.0 * 15.0 = 15.0
        assert!((out.integral_acc - 15.0).abs() < 0.01);
        assert!((out.i_term - 15.0).abs() < 0.01);
    }

    #[test]
    fn test_integral_accumulates_negative() {
        let mut pid = PidController::new(0.0, 0.5, 0.0, 1000.0);
        pid.update(-4.0, false);
        pid.update(-4.0, false);
        let out = pid.update(-4.0, false);
        // integral = -12.0, i_term = 0.5 * -12.0 = -6.0
        assert!((out.integral_acc - (-12.0)).abs() < 0.01);
        assert!((out.i_term - (-6.0)).abs() < 0.01);
    }

    #[test]
    fn test_integral_clamped_to_limit() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, 10.0);
        // integral_limit = limit / ki = 10 / 1 = 10
        for _ in 0..100 {
            pid.update(100.0, false);
        }
        let out = pid.update(0.0, false);
        assert!(out.integral_acc <= 10.0, "integral_acc={}", out.integral_acc);
    }

    #[test]
    fn test_reset_integral_clears_accumulator() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, 1000.0);
        pid.update(10.0, false);
        pid.update(10.0, false);
        pid.reset_integral();
        let out = pid.update(0.0, false);
        assert!((out.integral_acc).abs() < 0.01);
    }

    // ── D-term / derivative tests ──

    #[test]
    fn test_derivative_zero_on_first_update() {
        let mut pid = PidController::new(0.0, 0.0, 1.0, 1000.0);
        let out = pid.update(10.0, false);
        assert_eq!(out.d_term, 0.0, "First update derivative should be 0");
    }

    #[test]
    fn test_derivative_responds_to_change() {
        let mut pid = PidController::new(0.0, 0.0, 1.0, 1000.0);
        pid.update(0.0, false);
        let out = pid.update(10.0, false);
        // derivative = 10.0 - 0.0 = 10.0, d_term = 1.0 * 10.0
        assert!((out.d_term - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_derivative_negative_on_decreasing_error() {
        let mut pid = PidController::new(0.0, 0.0, 2.0, 1000.0);
        pid.update(20.0, false);
        let out = pid.update(10.0, false);
        // derivative = 10 - 20 = -10, d_term = 2.0 * -10.0 = -20.0
        assert!((out.d_term - (-20.0)).abs() < 0.01);
    }

    #[test]
    fn test_derivative_zero_on_constant_error() {
        let mut pid = PidController::new(0.0, 0.0, 5.0, 1000.0);
        pid.update(7.0, false);
        let out = pid.update(7.0, false);
        assert!((out.d_term).abs() < 0.01, "Constant error => zero derivative");
    }

    // ── Anti-windup tests ──

    #[test]
    fn test_anti_windup_flag_when_pool_empty_and_negative_error() {
        let mut pid = PidController::new(2.0, 1.0, 0.0, 100.0);
        let out = pid.update(-10.0, true);
        assert!(out.anti_windup_active);
    }

    #[test]
    fn test_no_anti_windup_when_pool_has_items() {
        let mut pid = PidController::new(2.0, 1.0, 0.0, 100.0);
        let out = pid.update(-10.0, false);
        assert!(!out.anti_windup_active);
    }

    #[test]
    fn test_no_anti_windup_on_positive_error_empty_pool() {
        let mut pid = PidController::new(2.0, 1.0, 0.0, 100.0);
        let out = pid.update(10.0, true);
        assert!(!out.anti_windup_active);
    }

    #[test]
    fn test_anti_windup_prevents_integral_drift() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, 1000.0);
        for _ in 0..50 {
            pid.update(-10.0, true); // anti-windup active
        }
        // Integral should be ~0 because anti-windup blocked accumulation
        let out = pid.update(0.0, true);
        assert!(out.integral_acc.abs() < 0.01, "integral should not drift: {}", out.integral_acc);
    }

    // ── Full reset test ──

    #[test]
    fn test_full_reset() {
        let mut pid = PidController::new(2.0, 0.5, 1.0, 100.0);
        pid.update(10.0, false);
        pid.update(20.0, false);
        pid.reset();
        let out = pid.update(5.0, false);
        // After reset, derivative should be 0 (not initialized)
        assert_eq!(out.d_term, 0.0);
        // Integral should only contain this one update
        assert!((out.integral_acc - 5.0).abs() < 0.01);
    }

    // ── PidOutput Display test ──

    #[test]
    fn test_pid_output_display() {
        let mut pid = PidController::new(2.0, 0.1, 0.5, 100.0);
        let out = pid.update(10.0, false);
        let display = format!("{}", out);
        assert!(display.contains("error="));
        assert!(display.contains("Kp="));
        assert!(display.contains("P="));
        assert!(display.contains("I="));
        assert!(display.contains("D="));
    }

    // ── Convergence simulation ──

    #[test]
    fn test_pid_converges_to_zero_error() {
        let mut pid = PidController::new(0.5, 0.1, 0.2, 100.0);
        let mut simulated_value = 0.0;
        let target = 70.0;
        for _ in 0..200 {
            let error = target - simulated_value;
            let out = pid.update(error, false);
            simulated_value += out.clamped_output * 0.5; // simulate response
        }
        // Should converge near target
        assert!((simulated_value - target).abs() < 5.0, "value={}", simulated_value);
    }

    #[test]
    fn test_pid_all_three_terms_contribute() {
        let mut pid = PidController::new(1.0, 0.5, 0.3, 1000.0);
        pid.update(5.0, false); // init
        let out = pid.update(10.0, false);
        // P = 1.0 * 10 = 10.0
        // I = 0.5 * (5+10) = 7.5
        // D = 0.3 * (10-5) = 1.5
        assert!((out.p_term - 10.0).abs() < 0.01, "P={}", out.p_term);
        assert!((out.i_term - 7.5).abs() < 0.01, "I={}", out.i_term);
        assert!((out.d_term - 1.5).abs() < 0.01, "D={}", out.d_term);
    }
}
