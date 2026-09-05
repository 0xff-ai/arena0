# Sealed-bid Vickrey auction

`vickrey-auction` runs an integer-valued second-price auction. Participant 0
is the seller and every other participant is a bidder. The program supports
between three and nine participants.

The session parameters name the item and may set an integer reserve:

```json
{
  "item": "launch lot",
  "reserve": 50
}
```

Each bidder answers one `SubmitBid` callout with a JSON integer:

```json
120
```

The program commits every bid before it reveals any bid. A missing input,
commitment, or reveal leaves the auction pending. The guest has no clock and
does not apply an implicit timeout or forfeiture.

The highest bid at or above the reserve wins. The winner pays the second
highest qualifying bid. If only one bid qualifies, the winner pays the reserve,
or zero when no reserve exists. Jointly committed entropy selects the winner
when the highest qualifying bid is tied.

The receipt records the item, the reserve, all bidder values in canonical
participant order, and either the settlement or the no-sale result. It proves
agreement on those program facts. The program does not transfer money or prove
that an external payment occurred.
