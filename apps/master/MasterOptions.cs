namespace DigBlock.Master;

/// <summary>Bound from the "Master" configuration section.</summary>
public sealed class MasterOptions
{
    public ProbeOptions Probe { get; set; } = new();
    public TokenOptions Token { get; set; } = new();
}

public sealed class ProbeOptions
{
    public int IntervalSeconds { get; set; } = 30;
    public int TimeoutSeconds { get; set; } = 5;
    public int MaxStrikes { get; set; } = 3;
}

public sealed class TokenOptions
{
    public int TtlSeconds { get; set; } = 45;
}
