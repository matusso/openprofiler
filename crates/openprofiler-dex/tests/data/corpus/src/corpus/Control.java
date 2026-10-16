package corpus;

import java.util.Iterator;
import java.util.List;

public class Control {
    public int classify(int x) {
        if (x < 0) {
            return -1;
        } else if (x == 0) {
            return 0;
        } else if (x < 10) {
            return 1;
        }
        return 2;
    }

    public boolean both(String a, String b) {
        return a != null && b != null && a.equals(b);
    }

    public boolean either(int a, int b) {
        if (a > 0 || b > 0) {
            return true;
        }
        return false;
    }

    public int loops(int n) {
        int s = 0;
        int i = 0;
        while (i < n) {
            s += i;
            i++;
        }
        do {
            s--;
        } while (s > 100);
        for (int j = 0; j < n; j++) {
            if (j % 2 == 0) {
                continue;
            }
            if (j > 50) {
                break;
            }
            s += j;
        }
        return s;
    }

    public int find(int[][] grid, int target) {
        int found = -1;
        outer:
        for (int r = 0; r < grid.length; r++) {
            for (int c = 0; c < grid[r].length; c++) {
                if (grid[r][c] == target) {
                    found = r;
                    break outer;
                }
            }
        }
        return found;
    }

    public String dense(int d) {
        switch (d) {
            case 0:
                return "zero";
            case 1:
                return "one";
            case 2:
            case 3:
                return "few";
            default:
                return "many";
        }
    }

    public int sparse(int d) {
        int r;
        switch (d) {
            case 10:
                r = 1;
                break;
            case 1000:
                r = 2;
                break;
            case 100000:
                r = 3;
                break;
            default:
                r = 0;
        }
        return r;
    }

    public int strings(String s) {
        switch (s) {
            case "alpha":
                return 1;
            case "beta":
                return 2;
            default:
                return 0;
        }
    }

    public int count(List<String> items) {
        int n = 0;
        for (String s : items) {
            if (s.isEmpty()) {
                n++;
            }
        }
        Iterator<String> it = items.iterator();
        while (it.hasNext()) {
            if (it.next() == null) {
                it.remove();
            }
        }
        return n;
    }

    public int max(int a, int b) {
        return a > b ? a : b;
    }

    public String firstNonEmpty(String[] arr) {
        for (String s : arr) {
            if (s != null && !s.isEmpty()) {
                return s;
            }
        }
        return null;
    }
}
