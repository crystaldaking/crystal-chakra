namespace Chakra.Payments.Shared;

public static class Shared
{
    public static string NormalizePayment(this string value)
    {
        return value.Trim();
    }

    public static void RecordEvent(string name)
    {
        Console.WriteLine(name);
    }

    public static string SharedUniqueTarget(string value)
    {
        return value.Trim();
    }
}
