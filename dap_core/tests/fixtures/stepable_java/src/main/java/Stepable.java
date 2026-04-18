public class Stepable {

    static int add(int x, int y) {
        return x + y;
    }

    public static void main(String[] args) {
        int a = 10;
        int b = 20;
        int c = add(a, b);
        System.out.println("result=" + c);
    }
}
