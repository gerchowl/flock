any(.labels[].name | ascii_downcase; . == "tracking" or . == "epic")
or (.title | ascii_downcase | endswith("(tracking)"))
