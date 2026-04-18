public class OuterClass {
    public static void main(String[] args) {
        // Trigger anonymous class creation
        Runnable r = new Runnable() {
            @Override
            public void run() {
                System.out.println("anonymous");
            }
        };
        r.run();
        new OuterClass().new InnerClass().greet();
    }

    public class InnerClass {
        public void greet() {
            System.out.println("inner");
        }
    }
}
