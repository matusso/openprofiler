package matrix;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterChain;
import jakarta.servlet.FilterConfig;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;
import java.io.IOException;

/** A filter in front of the servlet: the request passes two filter chain calls. */
public class TimingFilter implements Filter {
    @Override
    public void init(FilterConfig config) {}

    @Override
    public void doFilter(ServletRequest req, ServletResponse resp, FilterChain chain)
            throws IOException, ServletException {
        chain.doFilter(req, resp);
    }

    @Override
    public void destroy() {}
}
